//! Session sealing with the negotiated AEAD.
//!
//! AES-128-GCM by default; ChaCha20-Poly1305 for peers without hardware AES.
//! Same 96-bit nonce, 16-byte tag, and AAD shape.
//!
//! Nonce is `salt (4 bytes) || sequence (8 bytes, BE)`. Reusing a
//! `(key, nonce)` pair is catastrophic. Host and client share one key+salt
//! and both count from 0, so `salt[0]`'s top bit is the sender's direction —
//! disjoint nonce spaces. Pairing supplies a fresh `(key, salt)` per session;
//! `Config::validate` rejects an all-zero key when `encrypt` is on.
//!
//! Sequence is also AAD, so a tampered on-wire seq fails the tag instead of
//! shifting the nonce. Anti-replay lives in `Session`, not here.
//!
//! `punktfunk/2` ([`MediaKeys`]) sends no key: each direction has its own secret from the TLS
//! exporter, packet `n` seals under key `n / MEDIA_KEY_PACKETS`, and the nonce is that key's IV
//! XOR `n`, as in QUIC. The packet's clear prefix is the AAD.

use crate::config::Role;
use crate::error::{PunktfunkError, Result};
use aes_gcm::aead::{Aead, AeadInOut, KeyInit, Payload};
use aes_gcm::Aes128Gcm;
use zeroize::Zeroize;

pub const TAG_LEN: usize = 16;

// CRYPTO_OVERHEAD and every in-place split assume both AEADs append TAG_LEN.
const _: () = assert!(std::mem::size_of::<aes_gcm::Tag>() == TAG_LEN);
const _: () = assert!(std::mem::size_of::<chacha20poly1305::Tag>() == TAG_LEN);

// Both backends use the same nonce, AAD, and detached tag.
mod chacha {
    #[cfg(feature = "chacha-aws-lc-rs")]
    mod imp {
        use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};

        use crate::crypto::TAG_LEN;
        use crate::error::{PunktfunkError, Result};

        pub struct Key(LessSafeKey);

        impl Key {
            pub fn new(key: &[u8; 32]) -> Self {
                debug_assert_eq!(CHACHA20_POLY1305.tag_len(), TAG_LEN);
                // `LessSafeKey` because the nonce is ours (`salt || seq`), not a counter the
                // library owns; uniqueness is this module's invariant, documented above.
                let key = UnboundKey::new(&CHACHA20_POLY1305, key).expect("32-byte ChaCha20 key");
                Key(LessSafeKey::new(key))
            }

            pub fn seal_in_place(
                &self,
                nonce: [u8; 12],
                aad: &[u8],
                plaintext: &mut [u8],
            ) -> Result<[u8; TAG_LEN]> {
                let tag = self
                    .0
                    .seal_in_place_separate_tag(
                        Nonce::assume_unique_for_key(nonce),
                        Aad::from(aad),
                        plaintext,
                    )
                    .map_err(|_| PunktfunkError::Crypto)?;
                let mut out = [0u8; TAG_LEN];
                out.copy_from_slice(tag.as_ref());
                Ok(out)
            }

            pub fn open_in_place(
                &self,
                nonce: [u8; 12],
                aad: &[u8],
                ciphertext: &mut [u8],
                tag: &[u8; TAG_LEN],
            ) -> Result<()> {
                self.0
                    .open_in_place_separate_tag(
                        Nonce::assume_unique_for_key(nonce),
                        Aad::from(aad),
                        tag,
                        ciphertext,
                    )
                    .map_err(|_| PunktfunkError::Crypto)?;
                Ok(())
            }
        }
    }

    #[cfg(not(feature = "chacha-aws-lc-rs"))]
    mod imp {
        use chacha20poly1305::aead::{AeadInOut, KeyInit};
        use chacha20poly1305::ChaCha20Poly1305;

        use crate::crypto::TAG_LEN;
        use crate::error::{PunktfunkError, Result};

        pub struct Key(ChaCha20Poly1305);

        impl Key {
            pub fn new(key: &[u8; 32]) -> Self {
                Key(ChaCha20Poly1305::new(key.into()))
            }

            pub fn seal_in_place(
                &self,
                nonce: [u8; 12],
                aad: &[u8],
                plaintext: &mut [u8],
            ) -> Result<[u8; TAG_LEN]> {
                let tag = self
                    .0
                    .encrypt_inout_detached((&nonce).into(), aad, plaintext.into())
                    .map_err(|_| PunktfunkError::Crypto)?;
                Ok(tag.into())
            }

            pub fn open_in_place(
                &self,
                nonce: [u8; 12],
                aad: &[u8],
                ciphertext: &mut [u8],
                tag: &[u8; TAG_LEN],
            ) -> Result<()> {
                self.0
                    .decrypt_inout_detached((&nonce).into(), aad, ciphertext.into(), tag.into())
                    .map_err(|_| PunktfunkError::Crypto)
            }
        }
    }

    pub use imp::Key;
}

/// Negotiated AEAD plus matching key. Mixed cipher/key sizes are unrepresentable.
/// ChaCha is 32 bytes (RFC 8439); offered when the peer advertised
/// [`VIDEO_CAP_CHACHA20`](crate::quic::VIDEO_CAP_CHACHA20).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionKey {
    Aes128Gcm([u8; 16]),
    ChaCha20Poly1305([u8; 32]),
}

impl SessionKey {
    pub fn cipher_name(&self) -> &'static str {
        match self {
            SessionKey::Aes128Gcm(_) => "aes-128-gcm",
            SessionKey::ChaCha20Poly1305(_) => "chacha20-poly1305",
        }
    }

    /// All-zero key. `Config::validate` rejects this when encryption is on.
    pub fn is_zero(&self) -> bool {
        match self {
            SessionKey::Aes128Gcm(k) => k == &[0u8; 16],
            SessionKey::ChaCha20Poly1305(k) => k == &[0u8; 32],
        }
    }
}

/// Redacts key bytes; `Config`'s `Debug` depends on that.
impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionKey::Aes128Gcm(_) => f.write_str("Aes128Gcm(<redacted>)"),
            SessionKey::ChaCha20Poly1305(_) => f.write_str("ChaCha20Poly1305(<redacted>)"),
        }
    }
}

/// Zeroizes key material in place; `Config`'s `Drop` depends on that.
impl Zeroize for SessionKey {
    fn zeroize(&mut self) {
        match self {
            SessionKey::Aes128Gcm(k) => k.zeroize(),
            SessionKey::ChaCha20Poly1305(k) => k.zeroize(),
        }
    }
}

// One SessionCrypto per session; boxing would chase a pointer on every seal/open.
#[allow(clippy::large_enum_variant)]
enum Cipher {
    Aes128Gcm(Aes128Gcm),
    ChaCha20Poly1305(chacha::Key),
}

impl Cipher {
    fn seal(&self, nonce: [u8; 12], aad: &[u8], plaintext: &mut [u8]) -> Result<[u8; TAG_LEN]> {
        match self {
            Cipher::Aes128Gcm(c) => Ok(c
                .encrypt_inout_detached((&nonce).into(), aad, plaintext.into())
                .map_err(|_| PunktfunkError::Crypto)?
                .into()),
            Cipher::ChaCha20Poly1305(c) => c.seal_in_place(nonce, aad, plaintext),
        }
    }

    fn open(
        &self,
        nonce: [u8; 12],
        aad: &[u8],
        ciphertext: &mut [u8],
        tag: &[u8; TAG_LEN],
    ) -> Result<()> {
        match self {
            Cipher::Aes128Gcm(c) => c
                .decrypt_inout_detached((&nonce).into(), aad, ciphertext.into(), tag.into())
                .map_err(|_| PunktfunkError::Crypto),
            Cipher::ChaCha20Poly1305(c) => c.open_in_place(nonce, aad, ciphertext, tag),
        }
    }
}

/// A session's AEAD: `punktfunk/1`'s one key and salt, or `punktfunk/2`'s per-direction media
/// secrets ([`SessionCrypto::media`]). Each side's methods refuse the other's layout.
pub struct SessionCrypto {
    kind: Kind,
}

// The v1 cipher stays inline, as `Cipher` does: boxing would chase a pointer on every seal.
#[allow(clippy::large_enum_variant)]
enum Kind {
    V1 {
        cipher: Cipher,
        /// This side's nonce salt (direction bit set).
        send_salt: [u8; 4],
        /// Peer's nonce salt (the other direction bit).
        recv_salt: [u8; 4],
    },
    V2 {
        send: MediaDir,
        recv: MediaDir,
    },
}

impl SessionCrypto {
    pub fn new(key: &SessionKey, salt: [u8; 4], role: Role) -> Self {
        let cipher = match key {
            // Compile-time `&[u8; N]` → `hybrid_array`; not runtime `from_slice`.
            SessionKey::Aes128Gcm(k) => Cipher::Aes128Gcm(Aes128Gcm::new(k.into())),
            SessionKey::ChaCha20Poly1305(k) => Cipher::ChaCha20Poly1305(chacha::Key::new(k)),
        };
        let own = direction(role);
        SessionCrypto {
            kind: Kind::V1 {
                cipher,
                send_salt: dir_salt(salt, own),
                recv_salt: dir_salt(salt, own ^ 1),
            },
        }
    }

    /// `punktfunk/2` media: this role seals with its own direction's keys and opens the peer's.
    pub fn media(keys: &MediaKeys, role: Role) -> Self {
        let (send, recv) = match role {
            Role::Host => (keys.host_to_client, keys.client_to_host),
            Role::Client => (keys.client_to_host, keys.host_to_client),
        };
        SessionCrypto {
            kind: Kind::V2 {
                send: MediaDir::new(keys.suite, send),
                recv: MediaDir::new(keys.suite, recv),
            },
        }
    }

    /// Whether this is `punktfunk/2` media crypto.
    pub fn is_media(&self) -> bool {
        matches!(self.kind, Kind::V2 { .. })
    }

    /// `punktfunk/2`: seal `buf` (`plaintext ‖ TAG_LEN scratch`) as packet `seq`, binding `aad`,
    /// the packet's clear prefix.
    pub fn seal_media(&self, seq: u64, aad: &[u8], buf: &mut [u8]) -> Result<()> {
        let Kind::V2 { send, .. } = &self.kind else {
            return Err(PunktfunkError::Crypto);
        };
        if buf.len() < TAG_LEN {
            return Err(PunktfunkError::BadPacket);
        }
        let key = send.key(seq);
        let split = buf.len() - TAG_LEN;
        let (plaintext, tag_slot) = buf.split_at_mut(split);
        let tag = key.cipher.seal(media_nonce(&key.iv, seq), aad, plaintext)?;
        tag_slot.copy_from_slice(&tag);
        Ok(())
    }

    /// `punktfunk/2`: open `buf` (`ciphertext ‖ tag`) of packet `seq` in place; the plaintext
    /// length. On failure the buffer's contents are unspecified.
    pub fn open_media(&self, seq: u64, aad: &[u8], buf: &mut [u8]) -> Result<usize> {
        let Kind::V2 { recv, .. } = &self.kind else {
            return Err(PunktfunkError::Crypto);
        };
        if buf.len() < TAG_LEN {
            return Err(PunktfunkError::BadPacket);
        }
        let key = recv.key(seq);
        let split = buf.len() - TAG_LEN;
        let (ciphertext, tag) = buf.split_at_mut(split);
        let tag: &[u8; TAG_LEN] = (&*tag).try_into().map_err(|_| PunktfunkError::Crypto)?;
        key.cipher
            .open(media_nonce(&key.iv, seq), aad, ciphertext, tag)?;
        Ok(split)
    }

    /// Seal `plaintext` for `seq`. Returns `ciphertext || tag`. `seq` is AAD.
    pub fn seal(&self, seq: u64, plaintext: &[u8]) -> Result<Vec<u8>> {
        let Kind::V1 {
            cipher, send_salt, ..
        } = &self.kind
        else {
            return Err(PunktfunkError::Crypto);
        };
        let nonce = nonce(*send_salt, seq);
        let aad = seq.to_be_bytes();
        let payload = Payload {
            msg: plaintext,
            aad: &aad,
        };
        match cipher {
            Cipher::Aes128Gcm(c) => c
                .encrypt((&nonce).into(), payload)
                .map_err(|_| PunktfunkError::Crypto),
            Cipher::ChaCha20Poly1305(c) => {
                let mut buf = Vec::with_capacity(plaintext.len() + TAG_LEN);
                buf.extend_from_slice(plaintext);
                let tag = c.seal_in_place(nonce, &aad, &mut buf)?;
                buf.extend_from_slice(&tag);
                Ok(buf)
            }
        }
    }

    /// Seal in place: `buf` is `[plaintext..][TAG_LEN scratch]`; returns
    /// `[ciphertext..][tag]`, byte-identical to [`seal`](Self::seal).
    pub fn seal_in_place(&self, seq: u64, buf: &mut [u8]) -> Result<()> {
        debug_assert!(buf.len() >= TAG_LEN);
        let Kind::V1 {
            cipher, send_salt, ..
        } = &self.kind
        else {
            return Err(PunktfunkError::Crypto);
        };
        let nonce = nonce(*send_salt, seq);
        let split = buf.len() - TAG_LEN;
        let (plaintext, tag_slot) = buf.split_at_mut(split);
        let aad = seq.to_be_bytes();
        let tag = match cipher {
            Cipher::Aes128Gcm(c) => c
                .encrypt_inout_detached((&nonce).into(), &aad, plaintext.into())
                .map_err(|_| PunktfunkError::Crypto)?
                .into(),
            Cipher::ChaCha20Poly1305(c) => c.seal_in_place(nonce, &aad, plaintext)?,
        };
        tag_slot.copy_from_slice(&tag);
        Ok(())
    }

    /// Open `ciphertext || tag` for `seq` (also AAD).
    pub fn open(&self, seq: u64, ciphertext: &[u8]) -> Result<Vec<u8>> {
        if ciphertext.len() < TAG_LEN {
            return Err(PunktfunkError::Crypto);
        }
        let Kind::V1 {
            cipher, recv_salt, ..
        } = &self.kind
        else {
            return Err(PunktfunkError::Crypto);
        };
        let nonce = nonce(*recv_salt, seq);
        let aad = seq.to_be_bytes();
        let payload = Payload {
            msg: ciphertext,
            aad: &aad,
        };
        match cipher {
            Cipher::Aes128Gcm(c) => c
                .decrypt((&nonce).into(), payload)
                .map_err(|_| PunktfunkError::Crypto),
            Cipher::ChaCha20Poly1305(_) => {
                let mut buf = ciphertext.to_vec();
                let n = self.open_in_place(seq, &mut buf)?;
                buf.truncate(n);
                Ok(buf)
            }
        }
    }

    /// Open in place: `buf` is `[ciphertext..][tag]`; on success plaintext
    /// occupies the first `len - TAG_LEN` bytes (returned).
    /// On failure, discard the buffer: its contents are unspecified.
    pub fn open_in_place(&self, seq: u64, buf: &mut [u8]) -> Result<usize> {
        if buf.len() < TAG_LEN {
            return Err(PunktfunkError::BadPacket);
        }
        let Kind::V1 {
            cipher, recv_salt, ..
        } = &self.kind
        else {
            return Err(PunktfunkError::Crypto);
        };
        let nonce = nonce(*recv_salt, seq);
        let split = buf.len() - TAG_LEN;
        let (ciphertext, tag) = buf.split_at_mut(split);
        let aad = seq.to_be_bytes();
        let tag: &[u8; TAG_LEN] = (&*tag).try_into().map_err(|_| PunktfunkError::Crypto)?;
        match cipher {
            Cipher::Aes128Gcm(c) => c
                .decrypt_inout_detached((&nonce).into(), &aad, ciphertext.into(), tag.into())
                .map_err(|_| PunktfunkError::Crypto)?,
            Cipher::ChaCha20Poly1305(c) => c.open_in_place(nonce, &aad, ciphertext, tag)?,
        }
        Ok(split)
    }
}

/// Packets one `punktfunk/2` media key seals before the next takes over. RFC 9001 §6.6 sets
/// 2^23 for AES-GCM confidentiality; ChaCha20-Poly1305 follows the same schedule.
/// cbindgen:ignore
pub const MEDIA_KEY_PACKETS: u64 = 1 << 23;

/// `punktfunk/2` media AEAD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaSuite {
    Aes128Gcm,
    ChaCha20Poly1305,
}

/// `punktfunk/2` media secrets, one per direction. Both ends derive them from the connection's
/// TLS exporter, so no key byte crosses the wire, and each packet's key follows from its number:
/// packet `n` seals under key `n / MEDIA_KEY_PACKETS`.
#[derive(Clone)]
pub struct MediaKeys {
    pub suite: MediaSuite,
    pub host_to_client: [u8; 32],
    pub client_to_host: [u8; 32],
}

impl MediaKeys {
    /// From 32 bytes of the connection's TLS exporter.
    pub fn derive(exporter: &[u8; 32], suite: MediaSuite) -> MediaKeys {
        let mut k = MediaKeys {
            suite,
            host_to_client: [0; 32],
            client_to_host: [0; 32],
        };
        expand(exporter, &[b"pf2 media h2c"], &mut k.host_to_client);
        expand(exporter, &[b"pf2 media c2h"], &mut k.client_to_host);
        k
    }
}

impl Drop for MediaKeys {
    fn drop(&mut self) {
        self.host_to_client.zeroize();
        self.client_to_host.zeroize();
    }
}

impl std::fmt::Debug for MediaKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MediaKeys({:?}, <redacted>)", self.suite)
    }
}

/// HKDF-Expand (RFC 5869) over HMAC-SHA256, one block, so `out` is at most 32 bytes. The
/// exporter output is already a uniform key, so there is no extract step.
fn expand(prk: &[u8; 32], info: &[&[u8]], out: &mut [u8]) {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac =
        <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(prk).expect("hmac takes any key length");
    for part in info {
        mac.update(part);
    }
    mac.update(&[1]);
    out.copy_from_slice(&mac.finalize().into_bytes()[..out.len()]);
}

/// One key index's AEAD and IV.
struct PacketKey {
    cipher: Cipher,
    iv: [u8; 12],
}

/// One direction of `punktfunk/2` media: its secret and the keys derived from it so far.
struct MediaDir {
    suite: MediaSuite,
    secret: [u8; 32],
    /// At most three indices: the one in use and its neighbours, for reorder across a change.
    keys: std::sync::Mutex<Vec<(u64, std::sync::Arc<PacketKey>)>>,
}

impl MediaDir {
    fn new(suite: MediaSuite, secret: [u8; 32]) -> MediaDir {
        MediaDir {
            suite,
            secret,
            keys: std::sync::Mutex::new(Vec::with_capacity(3)),
        }
    }

    fn key(&self, seq: u64) -> std::sync::Arc<PacketKey> {
        let index = seq / MEDIA_KEY_PACKETS;
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, k)) = keys.iter().find(|(i, _)| *i == index) {
            return k.clone();
        }
        let n = index.to_be_bytes();
        let mut iv = [0u8; 12];
        expand(&self.secret, &[b"pf2 iv", &n], &mut iv);
        let cipher = match self.suite {
            MediaSuite::Aes128Gcm => {
                let mut k = [0u8; 16];
                expand(&self.secret, &[b"pf2 key", &n], &mut k);
                let c = Cipher::Aes128Gcm(Aes128Gcm::new((&k).into()));
                k.zeroize();
                c
            }
            MediaSuite::ChaCha20Poly1305 => {
                let mut k = [0u8; 32];
                expand(&self.secret, &[b"pf2 key", &n], &mut k);
                let c = Cipher::ChaCha20Poly1305(chacha::Key::new(&k));
                k.zeroize();
                c
            }
        };
        let key = std::sync::Arc::new(PacketKey { cipher, iv });
        if keys.len() == 3 {
            let oldest = (0..3).min_by_key(|&i| keys[i].0).expect("three entries");
            keys.swap_remove(oldest);
        }
        keys.push((index, key.clone()));
        key
    }
}

impl Drop for MediaDir {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

/// `iv XOR (0u32 ‖ seq BE)`, as QUIC builds its packet nonces.
fn media_nonce(iv: &[u8; 12], seq: u64) -> [u8; 12] {
    let mut n = *iv;
    for (b, s) in n[4..].iter_mut().zip(seq.to_be_bytes()) {
        *b ^= s;
    }
    n
}

fn direction(role: Role) -> u8 {
    match role {
        Role::Host => 0,
        Role::Client => 1,
    }
}

/// Set `salt[0]`'s top bit to `dir` so the two directions never share a nonce.
fn dir_salt(mut salt: [u8; 4], dir: u8) -> [u8; 4] {
    salt[0] = (salt[0] & 0x7f) | (dir << 7);
    salt
}

fn nonce(salt: [u8; 4], seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&salt);
    n[4..].copy_from_slice(&seq.to_be_bytes());
    n
}

/// Fresh AES-128 key for pairing / control-plane.
pub fn random_key() -> [u8; 16] {
    let mut k = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut k);
    k
}

/// Fresh 32-byte ChaCha20-Poly1305 key (RFC 8439).
pub fn random_key32() -> [u8; 32] {
    let mut k = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut k);
    k
}

pub fn random_salt() -> [u8; 4] {
    let mut s = [0u8; 4];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One fresh key per negotiated cipher; every sealing test below must hold for both.
    fn both_keys() -> [SessionKey; 2] {
        [
            SessionKey::Aes128Gcm(random_key()),
            SessionKey::ChaCha20Poly1305(random_key32()),
        ]
    }

    // Cross-library checks keep the backend switch wire-compatible in both directions.
    #[cfg(feature = "chacha-aws-lc-rs")]
    #[test]
    fn chacha_matches_rustcrypto_bytes() {
        use chacha20poly1305::ChaCha20Poly1305;

        let key = [0x5a; 32];
        let salt = [0x93, 0x37, 0x42, 0x99];
        let host = SessionCrypto::new(&SessionKey::ChaCha20Poly1305(key), salt, Role::Host);
        let client = SessionCrypto::new(&SessionKey::ChaCha20Poly1305(key), salt, Role::Client);
        let reference = ChaCha20Poly1305::new((&key).into());

        for (sender, receiver, dir) in [(&host, &client, 0), (&client, &host, 1)] {
            for seq in [0, 1, 4242, u64::MAX] {
                for len in [0, 1, 15, 16, 17, 63, 64, 65, 255, 256, 257, 1408] {
                    let msg: Vec<u8> = (0..len).map(|i| i as u8).collect();
                    let nonce = nonce(dir_salt(salt, dir), seq);
                    let aad = seq.to_be_bytes();
                    let theirs = reference
                        .encrypt(
                            (&nonce).into(),
                            Payload {
                                msg: &msg,
                                aad: &aad,
                            },
                        )
                        .unwrap();
                    let ours = sender.seal(seq, &msg).unwrap();
                    assert_eq!(ours, theirs, "dir={dir} seq={seq} len={len}");
                    assert_eq!(
                        reference
                            .decrypt(
                                (&nonce).into(),
                                Payload {
                                    msg: &ours,
                                    aad: &aad
                                }
                            )
                            .unwrap(),
                        msg
                    );
                    let mut buf = msg.clone();
                    buf.resize(len + TAG_LEN, 0);
                    sender.seal_in_place(seq, &mut buf).unwrap();
                    assert_eq!(buf, theirs);
                    assert_eq!(receiver.open(seq, &theirs).unwrap(), msg);
                    let n = receiver.open_in_place(seq, &mut buf).unwrap();
                    assert_eq!(&buf[..n], msg);

                    for index in [0, theirs.len() - 1] {
                        let mut corrupted = theirs.clone();
                        corrupted[index] ^= 1;
                        assert!(receiver.open(seq, &corrupted).is_err());
                        assert!(receiver.open_in_place(seq, &mut corrupted).is_err());
                    }
                    let mut buf = theirs;
                    assert!(receiver.open_in_place(seq ^ 1, &mut buf).is_err());
                }
            }
        }
    }

    /// Each direction opens only what the other sealed, a packet opens only as its own
    /// number, and the key changes at the index boundary.
    #[test]
    fn media_keys_are_directional_and_change_by_index() {
        for suite in [MediaSuite::Aes128Gcm, MediaSuite::ChaCha20Poly1305] {
            let keys = MediaKeys::derive(&[9; 32], suite);
            assert_ne!(keys.host_to_client, keys.client_to_host);
            let host = SessionCrypto::media(&keys, Role::Host);
            let client = SessionCrypto::media(&keys, Role::Client);
            let aad = [0u8, 1, 2, 3, 4];
            let msg = b"one shard of video".to_vec();
            for seq in [
                0,
                MEDIA_KEY_PACKETS - 1,
                MEDIA_KEY_PACKETS,
                5 * MEDIA_KEY_PACKETS + 7,
            ] {
                let mut buf = msg.clone();
                buf.resize(msg.len() + TAG_LEN, 0);
                host.seal_media(seq, &aad, &mut buf).unwrap();
                let sealed = buf.clone();
                assert!(host.open_media(seq, &aad, &mut buf.clone()).is_err());
                assert!(client.open_media(seq + 1, &aad, &mut buf.clone()).is_err());
                assert!(client
                    .open_media(seq, &[0u8, 1, 2, 3, 5], &mut buf.clone())
                    .is_err());
                let n = client.open_media(seq, &aad, &mut buf).unwrap();
                assert_eq!(&buf[..n], &msg[..]);
                // The same plaintext under the next index's key seals differently.
                let mut next = msg.clone();
                next.resize(msg.len() + TAG_LEN, 0);
                host.seal_media(seq + MEDIA_KEY_PACKETS, &aad, &mut next)
                    .unwrap();
                assert_ne!(next, sealed);
            }
            assert!(
                host.seal(0, b"x").is_err(),
                "v1 sealing refuses media crypto"
            );
        }
    }

    #[test]
    fn truncated_tags_preserve_error_kinds() {
        for key in both_keys() {
            let crypto = SessionCrypto::new(&key, [0; 4], Role::Client);
            for len in 0..TAG_LEN {
                let mut buf = vec![0; len];
                assert!(matches!(crypto.open(0, &buf), Err(PunktfunkError::Crypto)));
                assert!(matches!(
                    crypto.open_in_place(0, &mut buf),
                    Err(PunktfunkError::BadPacket)
                ));
            }
        }
    }

    #[test]
    fn seal_open_roundtrip_cross_direction() {
        for key in both_keys() {
            let salt = random_salt();
            let host = SessionCrypto::new(&key, salt, Role::Host);
            let client = SessionCrypto::new(&key, salt, Role::Client);

            let msg = b"the quick brown fox";
            let sealed = host.seal(42, msg).unwrap();
            assert_ne!(&sealed[..msg.len()], &msg[..]);
            assert_eq!(sealed.len(), msg.len() + TAG_LEN);
            assert_eq!(client.open(42, &sealed).unwrap(), msg);

            assert!(client.open(43, &sealed).is_err());
            // Host open uses the peer salt, so it cannot open its own outbound packet.
            assert!(host.open(42, &sealed).is_err());
        }
    }

    #[test]
    fn directions_use_distinct_nonce_spaces() {
        for key in both_keys() {
            let salt = [0u8; 4]; // all-zero base salt must still separate the directions
            let host = SessionCrypto::new(&key, salt, Role::Host);
            let client = SessionCrypto::new(&key, salt, Role::Client);
            assert_ne!(
                host.seal(0, b"abc").unwrap(),
                client.seal(0, b"abc").unwrap()
            );
        }
    }

    #[test]
    fn open_in_place_matches_open_and_rejects_tampering() {
        for key in both_keys() {
            let salt = random_salt();
            let host = SessionCrypto::new(&key, salt, Role::Host);
            let client = SessionCrypto::new(&key, salt, Role::Client);
            for msg in [
                &b""[..],
                b"x",
                b"the quick brown fox jumps over 13 lazy dogs!!",
            ] {
                let sealed = host.seal(9, msg).unwrap();
                let mut buf = sealed.clone();
                let n = client.open_in_place(9, &mut buf).unwrap();
                assert_eq!(
                    &buf[..n],
                    msg,
                    "in-place open must be byte-identical to open"
                );
                let mut buf = sealed.clone();
                assert!(client.open_in_place(8, &mut buf).is_err());
                let mut buf = sealed.clone();
                let last = buf.len() - 1;
                buf[last] ^= 1;
                assert!(client.open_in_place(9, &mut buf).is_err());
            }
            let mut runt = vec![0u8; TAG_LEN - 1];
            assert!(client.open_in_place(0, &mut runt).is_err());
        }
    }

    #[test]
    fn seal_in_place_matches_seal_and_opens() {
        for key in both_keys() {
            let salt = random_salt();
            let host = SessionCrypto::new(&key, salt, Role::Host);
            let client = SessionCrypto::new(&key, salt, Role::Client);
            for msg in [
                &b""[..],
                b"x",
                b"the quick brown fox jumps over 13 lazy dogs!!",
            ] {
                let reference = host.seal(7, msg).unwrap();
                let mut buf = msg.to_vec();
                buf.resize(msg.len() + TAG_LEN, 0);
                host.seal_in_place(7, &mut buf).unwrap();
                assert_eq!(
                    buf, reference,
                    "in-place seal must be byte-identical to seal"
                );
                assert_eq!(client.open(7, &buf).unwrap(), msg);
            }
        }
    }

    #[test]
    fn ciphers_are_not_interchangeable() {
        // ChaCha key repeats the AES bytes so overlapping material still cannot interoperate.
        let salt = random_salt();
        let aes = SessionKey::Aes128Gcm([7u8; 16]);
        let chacha = SessionKey::ChaCha20Poly1305([7u8; 32]);
        let sealed = SessionCrypto::new(&aes, salt, Role::Host)
            .seal(1, b"cross-cipher")
            .unwrap();
        assert!(SessionCrypto::new(&chacha, salt, Role::Client)
            .open(1, &sealed)
            .is_err());
        let sealed = SessionCrypto::new(&chacha, salt, Role::Host)
            .seal(1, b"cross-cipher")
            .unwrap();
        assert!(SessionCrypto::new(&aes, salt, Role::Client)
            .open(1, &sealed)
            .is_err());
    }

    #[test]
    fn session_key_zero_check_and_debug_redaction() {
        assert!(SessionKey::Aes128Gcm([0u8; 16]).is_zero());
        assert!(SessionKey::ChaCha20Poly1305([0u8; 32]).is_zero());
        assert!(!SessionKey::Aes128Gcm([1u8; 16]).is_zero());
        assert!(!SessionKey::ChaCha20Poly1305([1u8; 32]).is_zero());
        for key in both_keys() {
            let dbg = format!("{key:?}");
            assert!(dbg.contains("<redacted>"), "{dbg}");
        }
        let mut k = SessionKey::ChaCha20Poly1305([9u8; 32]);
        k.zeroize();
        assert!(k.is_zero());
    }
}
