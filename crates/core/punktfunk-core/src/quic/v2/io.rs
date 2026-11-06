//! v2 streams: every stream opens with its type varint, then carries `type ‖ len ‖ body` frames.
//!
//! Generic over `AsyncRead`/`AsyncWrite`, so the same frames run on a quinn stream or a
//! WebTransport one. [`FrameReader`] keeps a partial frame in its buffer, so a
//! read dropped under `select!` or a timeout resumes where it stopped. A frame's length is held
//! against its type's bound ([`max_body`]) before the body is buffered.

use super::field::{get_varint, put_varint, split_frame};
use super::msg::{ProfileUnlock, Zeroizing};
use super::registry::{max_body, MSG_PROFILE_UNLOCK};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

fn invalid(what: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what)
}

/// Cancel-safe reader of v2 frames on one stream. A `ProfileUnlock` crosses it, so every byte
/// it buffered or held is wiped once handed out, when its buffer moves, and on drop.
pub struct FrameReader<R> {
    recv: R,
    /// Bytes read and not yet returned; may hold the start of the next frame.
    buf: Zeroizing<Vec<u8>>,
    /// Frames a reader set aside for whoever reads next ([`Self::hold`]).
    held: std::collections::VecDeque<(u64, Zeroizing<Vec<u8>>)>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(recv: R) -> Self {
        FrameReader {
            recv,
            buf: Zeroizing::default(),
            held: std::collections::VecDeque::new(),
        }
    }

    /// Hand a frame back: the next read returns it first. For a reader that waits on one type
    /// while another arrives.
    pub fn hold(&mut self, (ty, body): (u64, Vec<u8>)) {
        self.held.push_back((ty, Zeroizing::new(body)));
    }

    /// The next frame as `(type, body)`. A clean end of stream between frames is
    /// `UnexpectedEof` like one inside a frame; the caller tells them apart by what it expected.
    /// A `ProfileUnlock` is wiped and skipped: only [`Self::read_profile_unlock`] returns one.
    pub async fn read_frame(&mut self) -> std::io::Result<(u64, Vec<u8>)> {
        loop {
            let (ty, mut body) = self.next_frame().await?;
            if ty != MSG_PROFILE_UNLOCK {
                return Ok((ty, std::mem::take(&mut *body)));
            }
            tracing::debug!("skipped a profile unlock no hello announced");
        }
    }

    /// The `ProfileUnlock` a hello with [`unlock_follows`] promised, by `deadline`. Call it
    /// right after that hello, before clock sync. Any other frame, a malformed unlock, the
    /// stream's end or the deadline is an error, and the frame it read is wiped.
    ///
    /// [`unlock_follows`]: super::hello::ClientHello::unlock_follows
    pub async fn read_profile_unlock(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> std::io::Result<ProfileUnlock> {
        let late =
            || std::io::Error::new(std::io::ErrorKind::TimedOut, "no profile unlock in time");
        let (ty, body) = tokio::time::timeout_at(deadline, self.next_frame())
            .await
            .map_err(|_| late())??;
        if ty != MSG_PROFILE_UNLOCK {
            return Err(invalid("another frame where a profile unlock was promised"));
        }
        super::msg::decode::<ProfileUnlock>(ty, &body).map_err(|_| invalid("bad profile unlock"))
    }

    /// The next frame, held ones first, with a body that wipes on drop.
    async fn next_frame(&mut self) -> std::io::Result<(u64, Zeroizing<Vec<u8>>)> {
        use zeroize::Zeroize;
        if let Some(frame) = self.held.pop_front() {
            return Ok(frame);
        }
        loop {
            let next = split_frame(&self.buf, max_body)
                .map_err(|_| invalid("v2 frame over its type's bound"))?;
            if let Some((ty, body, n)) = next {
                let body = Zeroizing::new(body.to_vec());
                self.buf[..n].zeroize();
                self.buf.drain(..n);
                // `drain` moved the rest down; the `n` bytes past the new end are a stale copy.
                self.buf.spare_capacity_mut()[..n].zeroize();
                return Ok((ty, body));
            }
            let mut chunk = Zeroizing::new([0u8; 4096]);
            // `read` reports only the bytes it returns, and they land in `buf` before the next
            // await: a dropped future loses nothing.
            match self.recv.read(&mut chunk[..]).await? {
                0 => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "v2 stream ended",
                    ))
                }
                n => grow_wiped(&mut self.buf, &chunk[..n]),
            }
        }
    }

    pub fn into_inner(self) -> R {
        self.recv
    }
}

/// Append `more` to `buf`, wiping the old allocation when it has to move.
fn grow_wiped(buf: &mut Vec<u8>, more: &[u8]) {
    use zeroize::Zeroize;
    if buf.capacity() - buf.len() < more.len() {
        let mut moved = Vec::with_capacity((buf.len() + more.len()).max(2 * buf.capacity()));
        moved.extend_from_slice(buf);
        buf.zeroize();
        *buf = moved;
    }
    buf.extend_from_slice(more);
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

    fn unlock(kind: u8) -> ProfileUnlock {
        ProfileUnlock {
            kind,
            secret: Zeroizing::new(b"open sesame".to_vec()),
            stay_signed_in: false,
            enroll_alg: crate::quic::v2::msg::ENROLL_NONE,
            enroll_key: vec![],
        }
    }

    fn soon(ms: u64) -> tokio::time::Instant {
        tokio::time::Instant::now() + std::time::Duration::from_millis(ms)
    }

    /// A promised unlock reads, a held one too, and one no hello announced never reaches
    /// `read_frame`.
    #[tokio::test]
    async fn a_promised_unlock_reads_and_a_stray_one_is_skipped() {
        use crate::quic::v2::msg::UNLOCK_PASSWORD;
        let (mut tx, rx) = tokio::io::duplex(4096);
        let mut reader = FrameReader::new(rx);
        tx.write_all(&unlock(UNLOCK_PASSWORD).encode())
            .await
            .unwrap();
        let got = reader.read_profile_unlock(soon(1000)).await.unwrap();
        assert_eq!(&got.secret[..], b"open sesame");
        reader.hold((MSG_PROFILE_UNLOCK, got.fields().into_body()));
        let held = reader.read_profile_unlock(soon(1000)).await.unwrap();
        assert_eq!(held.kind, UNLOCK_PASSWORD);

        tx.write_all(&unlock(UNLOCK_PASSWORD).encode())
            .await
            .unwrap();
        tx.write_all(&SetBitrate { bitrate_kbps: 9 }.encode_v2())
            .await
            .unwrap();
        let (ty, _) = reader.read_frame().await.unwrap();
        assert_eq!(ty, SetBitrate::TYPE);
    }

    /// Another frame first, a malformed unlock, half a frame by the deadline and the stream's
    /// end are each refused.
    #[tokio::test]
    async fn a_promised_unlock_refuses_anything_else() {
        use std::io::ErrorKind;
        let (mut tx, rx) = tokio::io::duplex(4096);
        let mut reader = FrameReader::new(rx);
        tx.write_all(&SetBitrate { bitrate_kbps: 9 }.encode_v2())
            .await
            .unwrap();
        let other = reader.read_profile_unlock(soon(1000)).await.unwrap_err();
        assert_eq!(other.kind(), ErrorKind::InvalidData);

        tx.write_all(&unlock(9).encode()).await.unwrap();
        let bad = reader.read_profile_unlock(soon(1000)).await.unwrap_err();
        assert_eq!(bad.kind(), ErrorKind::InvalidData);

        let frame = unlock(crate::quic::v2::msg::UNLOCK_PASSWORD).encode();
        tx.write_all(&frame[..5]).await.unwrap();
        let late = reader.read_profile_unlock(soon(20)).await.unwrap_err();
        assert_eq!(late.kind(), ErrorKind::TimedOut);

        drop(tx);
        let ended = reader.read_profile_unlock(soon(1000)).await.unwrap_err();
        assert_eq!(ended.kind(), ErrorKind::UnexpectedEof);
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
