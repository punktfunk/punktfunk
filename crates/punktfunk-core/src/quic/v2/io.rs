//! v2 streams: every stream opens with its type varint, then carries `type ‖ len ‖ body` frames.
//!
//! Generic over `AsyncRead`/`AsyncWrite`, as [`crate::quic::io`] is, so the same frames run on a
//! quinn stream or a WebTransport one. [`FrameReader`] keeps a partial frame in its buffer, so a
//! read dropped under `select!` or a timeout resumes where it stopped. A frame's length is held
//! against its type's bound ([`max_body`]) before the body is buffered.

use super::field::{get_varint, put_varint, split_frame};
use super::registry::max_body;
use super::translate::{RxEdge, TxEdge};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

fn invalid(what: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what)
}

/// Cancel-safe reader of v2 frames on one stream.
pub struct FrameReader<R> {
    recv: R,
    /// Bytes read and not yet returned; may hold the start of the next frame.
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(recv: R) -> Self {
        FrameReader {
            recv,
            buf: Vec::new(),
        }
    }

    /// The next frame as `(type, body)`. A clean end of stream between frames is
    /// `UnexpectedEof` like one inside a frame; the caller tells them apart by what it expected.
    pub async fn read_frame(&mut self) -> std::io::Result<(u64, Vec<u8>)> {
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

/// A control stream's read half on the v2 wire, read as `punktfunk/1`: each v2 frame becomes
/// the `u16 ‖ message` bytes [`crate::quic::io::MsgReader`] expects, through `edge`. A frame
/// with no v1 form is skipped.
pub struct V2Reader<R> {
    inner: R,
    edge: std::sync::Arc<std::sync::Mutex<RxEdge>>,
    /// v2 bytes read and not yet framed.
    wire: Vec<u8>,
    /// v1 bytes framed and not yet read.
    out: Vec<u8>,
}

impl<R> V2Reader<R> {
    pub fn new(inner: R, edge: std::sync::Arc<std::sync::Mutex<RxEdge>>) -> Self {
        V2Reader {
            inner,
            edge,
            wire: Vec::new(),
            out: Vec::new(),
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for V2Reader<R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;
        let this = self.get_mut();
        loop {
            if !this.out.is_empty() {
                let n = this.out.len().min(buf.remaining());
                buf.put_slice(&this.out[..n]);
                this.out.drain(..n);
                return Poll::Ready(Ok(()));
            }
            let next = split_frame(&this.wire, max_body)
                .map_err(|_| invalid("v2 frame over its type's bound"))?;
            if let Some((ty, body, n)) = next {
                let msg = this
                    .edge
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .to_v1(ty, body);
                this.wire.drain(..n);
                if let Some(m) = msg {
                    let len = u16::try_from(m.len()).map_err(|_| invalid("message over 64 KiB"))?;
                    this.out.extend_from_slice(&len.to_le_bytes());
                    this.out.extend_from_slice(&m);
                }
                continue;
            }
            let mut chunk = [0u8; 4096];
            let mut rb = tokio::io::ReadBuf::new(&mut chunk);
            match std::pin::Pin::new(&mut this.inner).poll_read(cx, &mut rb) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) if rb.filled().is_empty() => {
                    return Poll::Ready(if this.wire.is_empty() {
                        Ok(())
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "v2 stream ended mid-frame",
                        ))
                    });
                }
                Poll::Ready(Ok(())) => this.wire.extend_from_slice(rb.filled()),
            }
        }
    }
}

/// A control stream's write half on the v2 wire, written as `punktfunk/1`: each `u16 ‖ message`
/// handed to it leaves as its v2 frame, through `edge`.
///
/// It takes whole messages, as [`crate::quic::io::write_msg`] writes them, and accepts one only
/// once its frame is fully written, so a message never waits inside the wrapper. A message with
/// no v2 form is consumed and dropped.
pub struct V2Writer<W> {
    inner: W,
    edge: std::sync::Arc<std::sync::Mutex<TxEdge>>,
    pending: Vec<u8>,
    sent: usize,
    /// v1 bytes the pending frame stands for, reported once it is out.
    consumed: usize,
}

impl<W> V2Writer<W> {
    pub fn new(inner: W, edge: std::sync::Arc<std::sync::Mutex<TxEdge>>) -> Self {
        V2Writer {
            inner,
            edge,
            pending: Vec::new(),
            sent: 0,
            consumed: 0,
        }
    }
}

impl<W: AsyncWrite + Unpin> V2Writer<W> {
    fn poll_pending(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;
        while self.sent < self.pending.len() {
            match std::pin::Pin::new(&mut self.inner).poll_write(cx, &self.pending[self.sent..]) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()));
                }
                Poll::Ready(Ok(n)) => self.sent += n,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            }
        }
        self.pending.clear();
        self.sent = 0;
        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for V2Writer<W> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::task::Poll;
        let this = self.get_mut();
        if this.pending.is_empty() {
            let whole = buf
                .get(..2)
                .map(|l| 2 + u16::from_le_bytes([l[0], l[1]]) as usize)
                .filter(|&n| n <= buf.len());
            let Some(n) = whole else {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "v2 writer takes whole messages",
                )));
            };
            let frame = this
                .edge
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .to_v2(&buf[2..n]);
            match frame {
                Some(f) => {
                    this.pending = f;
                    this.consumed = n;
                }
                None => return Poll::Ready(Ok(n)),
            }
        }
        match this.poll_pending(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(std::mem::take(&mut this.consumed))),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        std::task::ready!(this.poll_pending(cx))?;
        std::pin::Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        std::task::ready!(this.poll_pending(cx))?;
        std::pin::Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::v2::msg::V2Message;
    use crate::quic::v2::registry::{MSG_CURSOR_SHAPE, STREAM_CONTROL, STREAM_TRANSFER};
    use crate::quic::{PipelineGap, SetBitrate};

    /// A frame type from a newer peer is skipped whole; the message after it still arrives.
    #[tokio::test]
    async fn a_reader_skips_a_frame_type_it_does_not_know() {
        let (mut tx, rx) = tokio::io::duplex(4096);
        let mut reader = V2Reader::new(rx, Default::default());
        let mut unknown = Vec::new();
        put_varint(&mut unknown, 0x3F00);
        put_varint(&mut unknown, 300);
        unknown.extend_from_slice(&[0xAB; 300]);
        tx.write_all(&unknown).await.unwrap();
        tx.write_all(&SetBitrate { bitrate_kbps: 7 }.encode_v2())
            .await
            .unwrap();
        let msg = crate::quic::io::read_msg(&mut reader).await.unwrap();
        assert_eq!(SetBitrate::decode(&msg).unwrap().bitrate_kbps, 7);
    }

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

    /// v1 messages written through the writer come out of a reader as the same v1 bytes, and
    /// the wire between them is v2.
    #[tokio::test]
    async fn v1_messages_cross_a_v2_stream() {
        use crate::quic::io::{write_msg, MsgReader};
        use crate::quic::{ClockProbe, Reconfigure, SetBitrate};
        let (a, b) = tokio::io::duplex(64);
        let tx = std::sync::Arc::new(std::sync::Mutex::new(TxEdge::default()));
        let rx = std::sync::Arc::new(std::sync::Mutex::new(RxEdge::default()));
        let mut w = V2Writer::new(a, tx);
        let mut r = MsgReader::new(V2Reader::new(b, rx));
        let msgs = vec![
            SetBitrate { bitrate_kbps: 7 }.encode(),
            Reconfigure {
                mode: crate::config::Mode {
                    width: 800,
                    height: 600,
                    refresh_hz: 60,
                },
            }
            .encode(),
            b"PKFc\x7f".to_vec(),
            ClockProbe { t1_ns: 3 }.encode(),
        ];
        let writer = tokio::spawn(async move {
            for m in &msgs {
                write_msg(&mut w, m).await.unwrap();
            }
            msgs
        });
        let sent = writer.await.unwrap();
        for m in sent.iter().filter(|m| m[4] != 0x7f) {
            assert_eq!(&r.read_msg().await.unwrap(), m);
        }
    }
}
