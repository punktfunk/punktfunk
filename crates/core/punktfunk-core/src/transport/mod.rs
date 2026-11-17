//! Pluggable packet I/O. The hot path calls the [`Transport`] methods directly — no async
//! runtime is involved.

/// Interface kinds, as [`ifinfo`] reads them and the wire carries them
/// ([`crate::quic::HostLink`], [`crate::quic::LinkFacts`]).
pub const IFACE_KIND_UNKNOWN: u8 = 0;
pub const IFACE_KIND_ETHERNET: u8 = 1;
pub const IFACE_KIND_WIFI: u8 = 2;
/// Loopback, a tunnel, a bridge: a link with no wire of its own.
pub const IFACE_KIND_OTHER: u8 = 3;

/// One end's interface as its OS reports it: an `IFACE_KIND_*` and a speed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkFacts {
    pub kind: u8,
    /// `0` = not sampled.
    pub mbps: u32,
}

#[cfg(not(target_family = "wasm"))]
pub mod ifinfo;
mod loopback;
mod qos;
#[cfg(windows)]
mod qos_windows;
/// One socket for QUIC and `punktfunk/2` media.
/// cbindgen:ignore
#[cfg(feature = "quic")]
pub mod shared;
#[cfg(not(target_family = "wasm"))]
pub mod sockstat;
#[cfg(feature = "quic")]
mod udp;

pub use loopback::{loopback_drop_head, loopback_drop_tail, loopback_pair, LoopbackTransport};
pub use qos::{
    grow_socket_buffers, set_dscp_default, set_media_qos, set_media_qos_to, MediaClass, QosFlow,
};

/// A datagram transport. `recv` is non-blocking: `Ok(None)` means no packet
/// is available, so the decode/present thread never blocks here.
pub trait Transport: Send + Sync {
    /// Send one packet. `Ok(true)` = handed to the kernel; `Ok(false)` = dropped
    /// locally because the send buffer was full (`WouldBlock`) — FEC/keyframe
    /// recover; count it (`packets_send_dropped`). `Err` = a real send failure.
    fn send(&self, packet: &[u8]) -> std::io::Result<bool>;

    /// Send a frame's packets in as few syscalls as possible; returns how many
    /// the kernel accepted. The caller counts `packets.len() - sent` as send-buffer
    /// drops. The shared socket batches through quinn-udp; the default is a scalar
    /// `send` loop. A full send buffer stops early — same lossy contract as `send`.
    fn send_batch(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        let mut sent = 0;
        for p in packets {
            if self.send(p)? {
                sent += 1;
            }
        }
        Ok(sent)
    }

    /// Send equal-size packets via UDP GSO where available: one `sendmsg`, kernel
    /// splits into `gso_size` datagrams. Default is [`send_batch`](Self::send_batch).
    /// Same short-count contract as `send_batch`.
    fn send_gso(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        self.send_batch(packets)
    }

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>>;

    /// The next received datagrams in the buffer they arrived in, owned; `None` = none
    /// available (non-blocking). The default is one scalar [`recv`](Self::recv).
    fn recv_owned(&self) -> std::io::Result<Option<RecvBatch>> {
        Ok(self.recv()?.map(RecvBatch::single))
    }

    /// [`recv_owned`](Self::recv_owned) after waiting up to `timeout` for datagrams to
    /// arrive. The default sleeps the whole timeout, then polls once.
    fn wait_owned(&self, timeout: std::time::Duration) -> std::io::Result<Option<RecvBatch>> {
        std::thread::sleep(timeout);
        self.recv_owned()
    }
}

/// Received datagrams in one buffer: `buf[..len]`, one every `stride` bytes, the last possibly
/// shorter. A pooled batch hands its buffer back to the pool when dropped.
pub struct RecvBatch {
    buf: Vec<u8>,
    len: usize,
    stride: usize,
    home: Option<std::sync::Arc<BufPool>>,
}

impl RecvBatch {
    /// One datagram that owns its buffer.
    pub fn single(buf: Vec<u8>) -> RecvBatch {
        let len = buf.len();
        RecvBatch {
            buf,
            len,
            stride: len.max(1),
            home: None,
        }
    }

    /// The datagram starting at byte `off`, or `None` past the last.
    pub fn at(&self, off: usize) -> Option<std::ops::Range<usize>> {
        (off < self.len).then(|| off..(off + self.stride).min(self.len))
    }

    /// Datagrams from byte `off` to the end.
    pub fn remaining(&self, off: usize) -> usize {
        self.len.saturating_sub(off).div_ceil(self.stride)
    }

    pub fn bytes(&self, r: std::ops::Range<usize>) -> &[u8] {
        &self.buf[r]
    }

    pub fn bytes_mut(&mut self, r: std::ops::Range<usize>) -> &mut [u8] {
        &mut self.buf[r]
    }
}

impl Drop for RecvBatch {
    fn drop(&mut self) {
        if let Some(pool) = self.home.take() {
            pool.put(std::mem::take(&mut self.buf));
        }
    }
}

/// Receive buffers of one size, at most `max_live` of them ever allocated. Buffers come back
/// through [`RecvBatch`]'s drop and are never freed while the pool lives.
#[cfg_attr(not(feature = "quic"), allow(dead_code))]
pub(crate) struct BufPool {
    free: std::sync::Mutex<Vec<Vec<u8>>>,
    live: std::sync::atomic::AtomicUsize,
    max_live: usize,
    size: usize,
}

#[cfg_attr(not(feature = "quic"), allow(dead_code))]
impl BufPool {
    pub(crate) fn new(size: usize, max_live: usize) -> std::sync::Arc<BufPool> {
        std::sync::Arc::new(BufPool {
            free: std::sync::Mutex::new(Vec::new()),
            live: std::sync::atomic::AtomicUsize::new(0),
            max_live,
            size,
        })
    }

    /// A buffer of `size` bytes, or `None` while `max_live` are out.
    pub(crate) fn take(&self) -> Option<Vec<u8>> {
        use std::sync::atomic::Ordering;
        if let Some(b) = self.free.lock().unwrap_or_else(|e| e.into_inner()).pop() {
            return Some(b);
        }
        if self.live.fetch_add(1, Ordering::Relaxed) < self.max_live {
            return Some(vec![0; self.size]);
        }
        self.live.fetch_sub(1, Ordering::Relaxed);
        None
    }

    fn put(&self, buf: Vec<u8>) {
        self.free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(buf);
    }

    /// `buf[..len]` as a batch that returns `buf` here when dropped.
    pub(crate) fn lend(
        self: &std::sync::Arc<BufPool>,
        buf: Vec<u8>,
        len: usize,
        stride: usize,
    ) -> RecvBatch {
        RecvBatch {
            buf,
            len,
            stride: stride.max(1),
            home: Some(self.clone()),
        }
    }
}
