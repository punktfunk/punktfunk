//! v2 streams: every stream opens with its type varint, then carries `type ‖ len ‖ body` frames.
//!
//! Generic over `AsyncRead`/`AsyncWrite`, so the same frames run on a quinn stream or a
//! WebTransport one. [`FrameReader`] keeps a partial frame in its buffer, so a
//! read dropped under `select!` or a timeout resumes where it stopped. A frame's length is held
//! against its type's bound ([`max_body`]) before the body is buffered.

use super::field::{get_varint, put_varint, split_frame};
use super::registry::max_body;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

fn invalid(what: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what)
}

/// Cancel-safe reader of v2 frames on one stream.
pub struct FrameReader<R> {
    recv: R,
    /// Bytes read and not yet returned; may hold the start of the next frame.
    buf: Vec<u8>,
    /// Frames a reader set aside for whoever reads next ([`Self::hold`]).
    held: std::collections::VecDeque<(u64, Vec<u8>)>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(recv: R) -> Self {
        FrameReader {
            recv,
            buf: Vec::new(),
            held: std::collections::VecDeque::new(),
        }
    }

    /// Hand a frame back: the next [`Self::read_frame`] returns it first. For a reader that
    /// waits on one type while another arrives.
    pub fn hold(&mut self, frame: (u64, Vec<u8>)) {
        self.held.push_back(frame);
    }

    /// The next frame as `(type, body)`. A clean end of stream between frames is
    /// `UnexpectedEof` like one inside a frame; the caller tells them apart by what it expected.
    pub async fn read_frame(&mut self) -> std::io::Result<(u64, Vec<u8>)> {
        if let Some(frame) = self.held.pop_front() {
            return Ok(frame);
        }
        loop {
            let next = split_frame(&self.buf, max_body)
                .map_err(|_| invalid("v2 frame over its type's bound"))?;
            if let Some((ty, body, n)) = next {
                let body = body.to_vec();
                self.buf.drain(..n);
                return Ok((ty, body));
            }
            let mut chunk = [0u8; 4096];
            // `read` reports only the bytes it returns, and they land in `buf` before the next
            // await: a dropped future loses nothing.
            match self.recv.read(&mut chunk).await? {
                0 => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "v2 stream ended",
                    ))
                }
                n => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    pub fn into_inner(self) -> R {
        self.recv
    }
}

/// Write `msg` as one whole frame.
pub async fn send<W: AsyncWrite + Unpin, M: super::msg::V2Message>(
    w: &mut W,
    msg: &M,
) -> std::io::Result<()> {
    w.write_all(&msg.encode_v2()).await
}

/// Write one whole frame (`type ‖ len ‖ body`, as [`super::msg::V2Message::encode_v2`] makes).
pub async fn write_frame<W: AsyncWrite + Unpin>(send: &mut W, frame: &[u8]) -> std::io::Result<()> {
    send.write_all(frame).await
}

/// Open a stream's first bytes: its type.
pub async fn write_stream_type<W: AsyncWrite + Unpin>(
    send: &mut W,
    ty: u64,
) -> std::io::Result<()> {
    let mut b = Vec::with_capacity(8);
    put_varint(&mut b, ty);
    send.write_all(&b).await
}

/// Exactly one frame, reading nothing past it: for a stream whose frame is followed by raw
/// bytes, such as a clipboard transfer's header and its data. Not cancel-safe.
pub async fn read_one_frame<R: AsyncRead + Unpin>(recv: &mut R) -> std::io::Result<(u64, Vec<u8>)> {
    let ty = read_stream_type(recv).await?;
    let len = read_stream_type(recv).await?;
    if len > max_body(ty) as u64 {
        return Err(invalid("v2 frame over its type's bound"));
    }
    let mut body = vec![0u8; len as usize];
    recv.read_exact(&mut body).await?;
    Ok((ty, body))
}

/// A stream's type, read before anything else on it. Not cancel-safe: read it once, at accept.
pub async fn read_stream_type<R: AsyncRead + Unpin>(recv: &mut R) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    recv.read_exact(&mut b[..1]).await?;
    let len = 1usize << (b[0] >> 6);
    recv.read_exact(&mut b[1..len]).await?;
    get_varint(&b[..len])
        .map(|(v, _)| v)
        .ok_or_else(|| invalid("bad stream type"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::v2::msg::V2Message;
    use crate::quic::v2::registry::{MSG_CURSOR_SHAPE, STREAM_CONTROL, STREAM_TRANSFER};
    use crate::quic::{PipelineGap, SetBitrate};

    /// A read dropped mid-frame loses nothing; the frame and the one after it still arrive.
    #[tokio::test]
    async fn frames_survive_a_cancelled_read_mid_frame() {
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut reader = FrameReader::new(rx);
        let a = SetBitrate { bitrate_kbps: 9 }.encode_v2();
        let b = PipelineGap { gap_ms: 401 }.encode_v2();
        tx.write_all(&a[..3]).await.unwrap();
        let cancelled =
            tokio::time::timeout(std::time::Duration::from_millis(20), reader.read_frame()).await;
        assert!(cancelled.is_err(), "a partial frame must not complete");
        tx.write_all(&a[3..]).await.unwrap();
        tx.write_all(&b).await.unwrap();
        let (ty, body) = reader.read_frame().await.unwrap();
        assert_eq!(SetBitrate::from_body(&body).unwrap().bitrate_kbps, 9);
        assert_eq!(ty, SetBitrate::TYPE);
        let (ty, body) = reader.read_frame().await.unwrap();
        assert_eq!(
            (ty, PipelineGap::from_body(&body).unwrap().gap_ms),
            (PipelineGap::TYPE, 401)
        );
    }

    #[tokio::test]
    async fn an_oversized_frame_is_refused_before_its_body_arrives() {
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut reader = FrameReader::new(rx);
        let mut head = Vec::new();
        put_varint(&mut head, MSG_CURSOR_SHAPE);
        put_varint(&mut head, (max_body(MSG_CURSOR_SHAPE) + 1) as u64);
        tx.write_all(&head).await.unwrap();
        let err = reader.read_frame().await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn stream_types_round_trip() {
        let (mut tx, mut rx) = tokio::io::duplex(64);
        for ty in [STREAM_CONTROL, STREAM_TRANSFER, 0x3FFF, 1 << 40] {
            write_stream_type(&mut tx, ty).await.unwrap();
            assert_eq!(read_stream_type(&mut rx).await.unwrap(), ty);
        }
    }
}
