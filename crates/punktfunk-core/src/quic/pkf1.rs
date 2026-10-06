//! What a client from before `punktfunk/2` still speaks: the PIN ceremony over ALPN `pkf1`, and
//! the refusal a browser page of that time reads. Each message is `u16 LE length ‖ payload`; the
//! payloads are the `encode_pkf1` / `decode_pkf1` forms of [`super::PairRequest`],
//! [`super::PairChallenge`], [`super::PairProof`], [`super::PairResult`] and [`super::Refused`].
//!
//! Not cancel-safe: the ceremony runs to completion on its own connection.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// `payload` framed: `u16 LE length ‖ payload`.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(2 + payload.len());
    b.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    b.extend_from_slice(payload);
    b
}

/// One framed message.
pub async fn read<R: AsyncRead + Unpin>(recv: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    recv.read_exact(&mut len).await?;
    let mut buf = vec![0u8; u16::from_le_bytes(len) as usize];
    recv.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn write<W: AsyncWrite + Unpin>(send: &mut W, payload: &[u8]) -> std::io::Result<()> {
    send.write_all(&frame(payload)).await
}
