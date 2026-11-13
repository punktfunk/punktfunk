//! Media sealing with the negotiated AEAD: AES-128-GCM by default, ChaCha20-Poly1305 for peers
//! without hardware AES. Same 96-bit nonce and 16-byte tag.
//!
//! No key crosses the wire ([`MediaKeys`]): each direction has its own secret from the TLS
//! exporter, packet `n` seals under key `n / MEDIA_KEY_PACKETS`, and the nonce is that key's IV
//! XOR `n`, as in QUIC. Distinct secrets per direction keep the two nonce spaces apart. The
//! packet's clear prefix is the AAD, so a tampered packet number fails the tag. Anti-replay
//! lives in `Session`, not here.

use crate::config::Role;
use crate::error::{PunktfunkError, Result};
use zeroize::Zeroize;

pub const TAG_LEN: usize = 16;

// WIRE_OVERHEAD and every in-place split assume both AEADs append TAG_LEN.
const _: () = assert!(std::mem::size_of::<aes_gcm::Tag>() == TAG_LEN);
const _: () = assert!(std::mem::size_of::<chacha20poly1305::Tag>() == TAG_LEN);

/// aws-lc-rs behind the detached-tag shape both suites share. `LessSafeKey` because the
/// nonce is ours (`salt || seq`), not a counter the library owns; uniqueness is this file's
/// invariant.
#[cfg(any(feature = "aes-aws-lc-rs", feature = "chacha-aws-lc-rs"))]
mod aws_lc {
    use aws_lc_rs::aead::{Aad, Algorithm, LessSafeKey, Nonce, UnboundKey};

    use crate::crypto::TAG_LEN;
    use crate::error::{PunktfunkError, Result};

    pub struct Key(LessSafeKey);

    impl Key {
        /// `key` is `alg`'s key length; both callers pass a fixed-size array.
        pub fn new(alg: &'static Algorithm, key: &[u8]) -> Self {
            debug_assert_eq!(alg.tag_len(), TAG_LEN);
            let key = UnboundKey::new(alg, key).expect("key length matches the algorithm");
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

        /// No out-of-place open on this backend: the copy is the one the in-place path makes.
        /// AES opens through here; `Cipher` copies for ChaCha itself.
        #[cfg(feature = "aes-aws-lc-rs")]
        pub fn open_into(
            &self,
            nonce: [u8; 12],
            aad: &[u8],
            ciphertext: &[u8],
            out: &mut [u8],
            tag: &[u8; TAG_LEN],
        ) -> Result<()> {
            if out.len() != ciphertext.len() {
                return Err(PunktfunkError::BadPacket);
            }
            out.copy_from_slice(ciphertext);
            self.open_in_place(nonce, aad, out, tag)
        }
    }
}

// Both backends use the same nonce, AAD, and detached tag.
mod chacha {
    #[cfg(feature = "chacha-aws-lc-rs")]
    mod imp {
        use crate::crypto::aws_lc;
        use crate::crypto::TAG_LEN;
        use crate::error::Result;

        pub struct Key(aws_lc::Key);

        impl Key {
            pub fn new(key: &[u8; 32]) -> Self {
                Key(aws_lc::Key::new(&aws_lc_rs::aead::CHACHA20_POLY1305, key))
            }

            pub fn seal_in_place(
                &self,
                nonce: [u8; 12],
                aad: &[u8],
                plaintext: &mut [u8],
            ) -> Result<[u8; TAG_LEN]> {
                self.0.seal_in_place(nonce, aad, plaintext)
            }

            pub fn open_in_place(
                &self,
                nonce: [u8; 12],
                aad: &[u8],
                ciphertext: &mut [u8],
                tag: &[u8; TAG_LEN],
            ) -> Result<()> {
                self.0.open_in_place(nonce, aad, ciphertext, tag)
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

mod aes {
    #[cfg(feature = "aes-aws-lc-rs")]
    mod imp {
        use crate::crypto::aws_lc;
        use crate::crypto::TAG_LEN;
        use crate::error::Result;

        pub struct Key(aws_lc::Key);

        impl Key {
            pub fn new(key: &[u8; 16]) -> Self {
                Key(aws_lc::Key::new(&aws_lc_rs::aead::AES_128_GCM, key))
            }

            pub fn seal_in_place(
                &self,
                nonce: [u8; 12],
                aad: &[u8],
                plaintext: &mut [u8],
            ) -> Result<[u8; TAG_LEN]> {
                self.0.seal_in_place(nonce, aad, plaintext)
            }

            pub fn open_in_place(
                &self,
                nonce: [u8; 12],
                aad: &[u8],
                ciphertext: &mut [u8],
                tag: &[u8; TAG_LEN],
            ) -> Result<()> {
                self.0.open_in_place(nonce, aad, ciphertext, tag)
            }

            pub fn open_into(
                &self,
                nonce: [u8; 12],
                aad: &[u8],
                ciphertext: &[u8],
                out: &mut [u8],
                tag: &[u8; TAG_LEN],
            ) -> Result<()> {
                self.0.open_into(nonce, aad, ciphertext, out, tag)
            }
        }
    }

    #[cfg(not(feature = "aes-aws-lc-rs"))]
    mod imp {
        use aes_gcm::aead::{AeadInOut, KeyInit};
        use aes_gcm::Aes128Gcm;

        use crate::crypto::TAG_LEN;
        use crate::error::{PunktfunkError, Result};

        pub struct Key(Aes128Gcm);

        impl Key {
            pub fn new(key: &[u8; 16]) -> Self {
                Key(Aes128Gcm::new(key.into()))
            }

            pub fn seal_in_place(
                &self,
                nonce: [u8; 12],
                aad: &[u8],
                plaintext: &mut [u8],
            ) -> Result<[u8; TAG_LEN]> {
                Ok(self
                    .0
                    .encrypt_inout_detached((&nonce).into(), aad, plaintext.into())
                    .map_err(|_| PunktfunkError::Crypto)?
                    .into())
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

            /// Open `ciphertext` into `out` (same length). On failure `out` is unspecified.
            pub fn open_into(
                &self,
                nonce: [u8; 12],
                aad: &[u8],
                ciphertext: &[u8],
                out: &mut [u8],
                tag: &[u8; TAG_LEN],
            ) -> Result<()> {
                let io = aes_gcm::aead::inout::InOutBuf::new(ciphertext, out)
                    .map_err(|_| PunktfunkError::BadPacket)?;
                self.0
                    .decrypt_inout_detached((&nonce).into(), aad, io, tag.into())
                    .map_err(|_| PunktfunkError::Crypto)
            }
        }
    }

    pub use imp::Key;
}

// One cipher per media key; boxing would chase a pointer on every seal/open.
#[allow(clippy::large_enum_variant)]
enum Cipher {
    Aes128Gcm(aes::Key),
    ChaCha20Poly1305(chacha::Key),
}

impl Cipher {
    fn seal(&self, nonce: [u8; 12], aad: &[u8], plaintext: &mut [u8]) -> Result<[u8; TAG_LEN]> {
        match self {
            Cipher::Aes128Gcm(c) => c.seal_in_place(nonce, aad, plaintext),
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
            Cipher::Aes128Gcm(c) => c.open_in_place(nonce, aad, ciphertext, tag),
            Cipher::ChaCha20Poly1305(c) => c.open_in_place(nonce, aad, ciphertext, tag),
        }
    }

    /// Open `ciphertext` into `out` (same length). On failure `out` is unspecified.
    fn open_into(
        &self,
        nonce: [u8; 12],
        aad: &[u8],
        ciphertext: &[u8],
        out: &mut [u8],
        tag: &[u8; TAG_LEN],
    ) -> Result<()> {
        match self {
            Cipher::Aes128Gcm(c) => c.open_into(nonce, aad, ciphertext, out, tag),
            // No out-of-place open on this backend: the copy is the one the in-place path makes.
            Cipher::ChaCha20Poly1305(c) => {
                if out.len() != ciphertext.len() {
                    return Err(PunktfunkError::BadPacket);
                }
                out.copy_from_slice(ciphertext);
                c.open_in_place(nonce, aad, out, tag)
            }
        }
    }
}

/// A session's media AEAD: this role seals with its own direction's keys and opens the peer's.
pub struct SessionCrypto {
    send: MediaDir,
    recv: MediaDir,
    /// Leading bytes of every packet left in the clear and bound as AAD.
    clear: usize,
}

impl SessionCrypto {
    /// `clear_placement`: the whole v2 header rides in the clear
    /// ([`FEATURE_CLEAR_PLACEMENT`](crate::quic::v2::registry::FEATURE_CLEAR_PLACEMENT)).
    pub fn media(keys: &MediaKeys, role: Role, clear_placement: bool) -> Self {
        let (send, recv) = match role {
            Role::Host => (keys.host_to_client, keys.client_to_host),
            Role::Client => (keys.client_to_host, keys.host_to_client),
        };
        SessionCrypto {
            send: MediaDir::new(keys.suite, send),
            recv: MediaDir::new(keys.suite, recv),
            clear: if clear_placement {
                crate::packet::V2_HEADER_LEN
            } else {
                crate::packet::V2_CLEAR_LEN
            },
        }
    }

    /// Leading bytes of a packet that are not sealed.
    pub fn clear_len(&self) -> usize {
        self.clear
    }

    /// Seal `wire` (`header ‖ body ‖ TAG_LEN scratch`) as packet `seq`; its clear prefix is the AAD.
    pub fn seal_wire(&self, seq: u64, wire: &mut [u8]) -> Result<()> {
        self.seal_wire_under(&self.send.key(seq), seq, wire)
    }

    /// [`Self::seal_wire`] for `wires[i]` as packet `seq_base + i`. The keys come from one
    /// [`KeyRun`], so two threads sealing halves of a frame share no per-packet state.
    pub fn seal_wires(&self, seq_base: u64, wires: &mut [Vec<u8>]) -> Result<()> {
        if wires.is_empty() {
            return Ok(());
        }
        let mut keys = KeyRun::new(&self.send, seq_base);
        for (i, wire) in wires.iter_mut().enumerate() {
            let seq = seq_base.wrapping_add(i as u64);
            self.seal_wire_under(keys.key(seq), seq, wire)?;
        }
        Ok(())
    }

    /// `key` is packet `seq`'s key, taken from the send direction by the caller.
    fn seal_wire_under(&self, key: &PacketKey, seq: u64, wire: &mut [u8]) -> Result<()> {
        let at = self.clear.min(wire.len());
        let (aad, rest) = wire.split_at_mut(at);
        key.seal(seq, aad, rest)
    }

    /// Open `wire` (`header ‖ sealed ‖ tag`) in place; the opened packet's length, header
    /// included. On failure the sealed part is unspecified.
    pub fn open_wire(&self, seq: u64, wire: &mut [u8]) -> Result<usize> {
        let at = self.clear.min(wire.len());
        let (aad, rest) = wire.split_at_mut(at);
        Ok(at + self.open_media(seq, aad, rest)?)
    }

    /// Open `wire`'s sealed body into `out`, which must be exactly that long; `wire` is left
    /// as received. On failure `out` is unspecified.
    pub fn open_wire_into(&self, seq: u64, wire: &[u8], out: &mut [u8]) -> Result<()> {
        if wire.len() < self.clear + TAG_LEN {
            return Err(PunktfunkError::BadPacket);
        }
        let (aad, rest) = wire.split_at(self.clear);
        let (ciphertext, tag) = rest.split_at(rest.len() - TAG_LEN);
        let tag: &[u8; TAG_LEN] = tag.try_into().map_err(|_| PunktfunkError::Crypto)?;
        let key = self.recv.key(seq);
        key.cipher
            .open_into(media_nonce(&key.iv, seq), aad, ciphertext, out, tag)
    }

    /// Seal `buf` (`plaintext ‖ TAG_LEN scratch`) as packet `seq`, binding `aad`, the packet's
    /// clear prefix.
    pub fn seal_media(&self, seq: u64, aad: &[u8], buf: &mut [u8]) -> Result<()> {
        self.send.key(seq).seal(seq, aad, buf)
    }

    /// Open `buf` (`ciphertext ‖ tag`) of packet `seq` in place; the plaintext length. On
    /// failure the buffer's contents are unspecified.
    pub fn open_media(&self, seq: u64, aad: &[u8], buf: &mut [u8]) -> Result<usize> {
        if buf.len() < TAG_LEN {
            return Err(PunktfunkError::BadPacket);
        }
        let key = self.recv.key(seq);
        let split = buf.len() - TAG_LEN;
        let (ciphertext, tag) = buf.split_at_mut(split);
        let tag: &[u8; TAG_LEN] = (&*tag).try_into().map_err(|_| PunktfunkError::Crypto)?;
        key.cipher
            .open(media_nonce(&key.iv, seq), aad, ciphertext, tag)?;
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

impl PacketKey {
    /// Seal `buf` (`plaintext ‖ TAG_LEN scratch`) as packet `seq`; `seq` must belong to this key.
    fn seal(&self, seq: u64, aad: &[u8], buf: &mut [u8]) -> Result<()> {
        let split = buf
            .len()
            .checked_sub(TAG_LEN)
            .ok_or(PunktfunkError::BadPacket)?;
        let (plaintext, tag_slot) = buf.split_at_mut(split);
        let tag = self
            .cipher
            .seal(media_nonce(&self.iv, seq), aad, plaintext)?;
        tag_slot.copy_from_slice(&tag);
        Ok(())
    }
}

/// The send keys for consecutive packets: the current one is held by this run alone and swapped
/// when a packet's own number names another index. The run is a local; no key outlives it.
struct KeyRun<'a> {
    dir: &'a MediaDir,
    index: u64,
    key: std::sync::Arc<PacketKey>,
}

impl<'a> KeyRun<'a> {
    fn new(dir: &'a MediaDir, seq: u64) -> Self {
        KeyRun {
            dir,
            index: seq / MEDIA_KEY_PACKETS,
            key: dir.key(seq),
        }
    }

    /// Packet `seq`'s key, whichever side of an index boundary it falls.
    fn key(&mut self, seq: u64) -> &PacketKey {
        let index = seq / MEDIA_KEY_PACKETS;
        if index != self.index {
            self.key = self.dir.key(seq);
            self.index = index;
        }
        &self.key
    }
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
                let c = Cipher::Aes128Gcm(aes::Key::new(&k));
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

#[cfg(test)]
mod tests {
    use super::*;

    // Cross-library check keeps the backend switch wire-compatible in both directions.
    #[cfg(feature = "aes-aws-lc-rs")]
    #[test]
    fn aes_matches_rustcrypto_bytes() {
        use aes_gcm::aead::{Aead, KeyInit, Payload};
        use aes_gcm::Aes128Gcm;

        let key = [0x5a; 16];
        let ours = Cipher::Aes128Gcm(aes::Key::new(&key));
        let reference = Aes128Gcm::new((&key).into());
        for seq in [0u64, 1, 4242, u64::MAX] {
            for len in [0, 1, 15, 16, 17, 63, 64, 65, 255, 256, 257, 1408] {
                let msg: Vec<u8> = (0..len).map(|i| i as u8).collect();
                let nonce = media_nonce(&[0x93; 12], seq);
                let aad = [0u8, 1, 2, 3, 4];
                let theirs = reference
                    .encrypt(
                        (&nonce).into(),
                        Payload {
                            msg: &msg,
                            aad: &aad,
                        },
                    )
                    .unwrap();
                let mut buf = msg.clone();
                let tag = ours.seal(nonce, &aad, &mut buf).unwrap();
                buf.extend_from_slice(&tag);
                assert_eq!(buf, theirs, "seq={seq} len={len}");
                let (ct, tag) = buf.split_at_mut(len);
                let tag: [u8; TAG_LEN] = (&*tag).try_into().unwrap();
                let mut out = vec![0u8; len];
                ours.open_into(nonce, &aad, ct, &mut out, &tag).unwrap();
                assert_eq!(out, msg);
                ours.open(nonce, &aad, ct, &tag).unwrap();
                assert_eq!(ct, &msg[..]);
                let mut corrupted = theirs.clone();
                corrupted[0] ^= 1;
                let (ct, tag) = corrupted.split_at_mut(len);
                let tag: [u8; TAG_LEN] = (&*tag).try_into().unwrap();
                assert!(len == 0 || ours.open(nonce, &aad, ct, &tag).is_err());
            }
        }
    }

    #[cfg(feature = "chacha-aws-lc-rs")]
    #[test]
    fn chacha_matches_rustcrypto_bytes() {
        use aes_gcm::aead::{Aead, KeyInit, Payload};
        use chacha20poly1305::ChaCha20Poly1305;

        let key = [0x5a; 32];
        let ours = Cipher::ChaCha20Poly1305(chacha::Key::new(&key));
        let reference = ChaCha20Poly1305::new((&key).into());
        for seq in [0u64, 1, 4242, u64::MAX] {
            for len in [0, 1, 15, 16, 17, 63, 64, 65, 255, 256, 257, 1408] {
                let msg: Vec<u8> = (0..len).map(|i| i as u8).collect();
                let nonce = media_nonce(&[0x93; 12], seq);
                let aad = [0u8, 1, 2, 3, 4];
                let theirs = reference
                    .encrypt(
                        (&nonce).into(),
                        Payload {
                            msg: &msg,
                            aad: &aad,
                        },
                    )
                    .unwrap();
                let mut buf = msg.clone();
                let tag = ours.seal(nonce, &aad, &mut buf).unwrap();
                buf.extend_from_slice(&tag);
                assert_eq!(buf, theirs, "seq={seq} len={len}");
                let (ct, tag) = buf.split_at_mut(len);
                let tag: [u8; TAG_LEN] = (&*tag).try_into().unwrap();
                ours.open(nonce, &aad, ct, &tag).unwrap();
                assert_eq!(ct, &msg[..]);
                let mut corrupted = theirs.clone();
                corrupted[0] ^= 1;
                let (ct, tag) = corrupted.split_at_mut(len);
                let tag: [u8; TAG_LEN] = (&*tag).try_into().unwrap();
                assert!(len == 0 || ours.open(nonce, &aad, ct, &tag).is_err());
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
            let host = SessionCrypto::media(&keys, Role::Host, false);
            let client = SessionCrypto::media(&keys, Role::Client, false);
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
            for len in 0..TAG_LEN {
                assert!(matches!(
                    client.open_media(0, &aad, &mut vec![0; len]),
                    Err(PunktfunkError::BadPacket)
                ));
            }
        }
    }

    /// A run seals each packet as the per-packet path does, across an index boundary and the
    /// packet number wrapping, and each opens only under its own number.
    #[test]
    fn seal_wires_matches_per_packet_across_key_changes() {
        for suite in [MediaSuite::Aes128Gcm, MediaSuite::ChaCha20Poly1305] {
            let keys = MediaKeys::derive(&[3; 32], suite);
            let by_run = SessionCrypto::media(&keys, Role::Host, true);
            let by_one = SessionCrypto::media(&keys, Role::Host, true);
            let client = SessionCrypto::media(&keys, Role::Client, true);
            let body = by_run.clear_len() + 40;
            for base in [
                0,
                MEDIA_KEY_PACKETS - 3,
                5 * MEDIA_KEY_PACKETS - 1,
                u64::MAX - 2,
            ] {
                let plain: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i; body + TAG_LEN]).collect();
                let mut run = plain.clone();
                let mut one = plain.clone();
                by_run.seal_wires(base, &mut run).unwrap();
                for (i, w) in one.iter_mut().enumerate() {
                    by_one.seal_wire(base.wrapping_add(i as u64), w).unwrap();
                }
                assert_eq!(run, one, "base {base}");
                for (i, w) in run.iter_mut().enumerate() {
                    let seq = base.wrapping_add(i as u64);
                    let sealed = w.clone();
                    let n = client.open_wire(seq, w).unwrap();
                    assert_eq!(&w[..n], &plain[i][..n], "seq {seq}");
                    let mut other = sealed.clone();
                    assert!(client
                        .open_wire(seq.wrapping_add(MEDIA_KEY_PACKETS), &mut other)
                        .is_err());
                }
            }
            by_run.seal_wires(0, &mut []).unwrap();
        }
    }

    /// A run's key is the one its packet number names: forward, backward and across the wrap.
    #[test]
    fn key_run_follows_the_packet_number() {
        let dir = MediaDir::new(MediaSuite::Aes128Gcm, [4; 32]);
        let mut run = KeyRun::new(&dir, 0);
        let seqs = [
            0,
            1,
            MEDIA_KEY_PACKETS - 1,
            MEDIA_KEY_PACKETS,
            2 * MEDIA_KEY_PACKETS + 5,
            MEDIA_KEY_PACKETS - 1,
            0,
            u64::MAX,
            0,
        ];
        for seq in seqs {
            assert_eq!(run.key(seq).iv, dir.key(seq).iv, "seq {seq}");
        }
        assert_ne!(dir.key(0).iv, dir.key(MEDIA_KEY_PACKETS).iv);
    }
}
