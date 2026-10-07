//! Pairing-plane AES-128-ECB (no padding), SHA-256, and PIN-key derivation. Distinct from
//! `punktfunk_core`'s AES-GCM data-plane sealing. SHA-256 is the Gen7+ digest (appversion
//! major ≥ 7). Spec: `design/research/gamestream-protocol-research.json` (serverinfo + pairing).

use aes::cipher::{Block, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use aes::Aes128;
use rand::Rng;
use sha2::{Digest, Sha256};

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::rng().fill_bytes(&mut b);
    b
}

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Constant-time equality. Length is not secret: a mismatch returns false immediately.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// PIN AES-128 key: `SHA-256(salt || pin)[..16]`. Salt first; PIN is ASCII.
pub fn pin_key(salt: &[u8; 16], pin: &str) -> [u8; 16] {
    let d = sha256(&[salt, pin.as_bytes()]);
    let mut k = [0u8; 16];
    k.copy_from_slice(&d[..16]);
    k
}

/// AES-128-ECB, no padding. Short input is zero-extended to a 16-byte multiple.
pub fn ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(key.into());
    let mut out = data.to_vec();
    let rem = out.len() % 16;
    if rem != 0 {
        out.resize(out.len() + (16 - rem), 0);
    }
    for chunk in out.chunks_mut(16) {
        let block: &mut Block<Aes128> = chunk.try_into().expect("16-byte block");
        cipher.encrypt_block(block);
    }
    out
}

/// AES-128-ECB, no padding. Bytes past the last whole block are dropped, not unpadded.
pub fn ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(key.into());
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        let mut block: Block<Aes128> = chunk.try_into().expect("16-byte block");
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    out
}
