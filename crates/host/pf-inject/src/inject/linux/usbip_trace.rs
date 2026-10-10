//! Byte-level tracing for the USB/IP transport (`PUNKTFUNK_USBIP_TRACE`).
//!
//! USB/IP is a framed byte stream on one TCP socket; each frame's length
//! lives inside the frame. A write whose byte count disagrees with its
//! header shifts every later read mid-frame, and the kernel reports the
//! error wherever it next fails to parse a PDU — not at the write that
//! desynced the stream.
//!
//! Off unless the env var is set. Wraps the socket and dumps both
//! directions plus per-call `(us,dir,offset,len)` records. Feed them to
//! `scripts/usbip-trace-analyse.py`. No parsing here: a tracer that
//! interprets can disagree with the wire the same way the code under test
//! does.
//!
//! ```text
//! PUNKTFUNK_USBIP_TRACE=/tmp/pad punktfunk-host pad-usbip-test --seconds 5
//! ```
//!
//! yields `/tmp/pad.<label>.rx` (kernel → us), `.tx` (us → kernel), `.idx`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// One prefix per attached device.
pub fn trace_prefix(label: &str) -> Option<String> {
    let base = std::env::var("PUNKTFUNK_USBIP_TRACE").ok()?;
    if base.is_empty() || base == "0" {
        return None;
    }
    // Human label; keep it filesystem-safe.
    let safe: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Some(format!("{base}.{safe}"))
}

/// Shared by the read and write halves.
struct Sink {
    rx: BufWriter<File>,
    tx: BufWriter<File>,
    idx: BufWriter<File>,
    rx_off: u64,
    tx_off: u64,
    start: Instant,
}

impl Sink {
    fn create(prefix: &str) -> std::io::Result<Self> {
        // Not `with_extension`: the label's own dots would collapse every
        // pad onto the same three files.
        let f = |ext: &str| File::create(format!("{prefix}.{ext}"));
        Ok(Sink {
            rx: BufWriter::new(f("rx")?),
            tx: BufWriter::new(f("tx")?),
            idx: BufWriter::new(f("idx")?),
            rx_off: 0,
            tx_off: 0,
            start: Instant::now(),
        })
    }

    /// `dir` is `r` (kernel → us) or `w` (us → kernel).
    fn record(&mut self, dir: char, bytes: &[u8]) {
        let (stream, off) = match dir {
            'r' => (&mut self.rx, &mut self.rx_off),
            _ => (&mut self.tx, &mut self.tx_off),
        };
        let at = *off;
        let _ = stream.write_all(bytes);
        *off += bytes.len() as u64;
        let us = self.start.elapsed().as_micros();
        let _ = writeln!(self.idx, "{us},{dir},{at},{}", bytes.len());
        // Per call: the socket dies under the failure, and a buffered tail
        // is the part that would be lost.
        let _ = stream.flush();
        let _ = self.idx.flush();
    }
}

/// Opening is separate from wrapping so a failed open does not already
/// hold the caller's socket.
pub struct TraceSink(Arc<Mutex<Sink>>);

/// Truncates any previous trace.
pub fn open_trace(prefix: &str) -> std::io::Result<TraceSink> {
    Ok(TraceSink(Arc::new(Mutex::new(Sink::create(prefix)?))))
}

/// Wraps at the call site, not inside the vendored server, so the traced
/// and untraced paths run the identical handler.
pub struct TracedIo<T> {
    inner: T,
    sink: Arc<Mutex<Sink>>,
}

impl<T> TracedIo<T> {
    pub fn wrap(inner: T, sink: TraceSink) -> Self {
        TracedIo {
            inner,
            sink: sink.0,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for TracedIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &r {
            let got = buf.filled()[before..].to_vec();
            // Zero-length ready read is EOF. Record it: which side closed
            // first is the whole question.
            if let Ok(mut s) = self.sink.lock() {
                s.record('r', &got);
            }
        }
        r
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for TracedIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &r {
            if let Ok(mut s) = self.sink.lock() {
                s.record('w', &buf[..*n]);
            }
        }
        r
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
