//! Pluggable packet I/O. The hot path calls [`Transport::send`] / [`Transport::recv`]
//! directly — no async runtime is involved.

/// Interface kinds, as [`ifinfo`] reads them and the wire carries them
/// ([`crate::quic::HostFacts`]).
pub const IFACE_KIND_UNKNOWN: u8 = 0;
pub const IFACE_KIND_ETHERNET: u8 = 1;
pub const IFACE_KIND_WIFI: u8 = 2;
/// Loopback, a tunnel, a bridge: a link with no wire of its own.
pub const IFACE_KIND_OTHER: u8 = 3;

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
mod udp;

pub use loopback::{loopback_pair, LoopbackTransport};
pub use qos::{grow_socket_buffers, set_dscp_default, set_media_qos, MediaClass, QosFlow};
/// Windows-only USO batch send for a caller that owns its connected socket (GameStream video).
#[cfg(target_os = "windows")]
pub use udp::send_uso_all;

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

    /// Turn GSO on or off for [`send_gso`](Self::send_gso). Default: nothing to switch.
    fn set_gso(&self, _on: bool) {}

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>>;

    /// Receive up to `out.len()` datagrams into caller-owned `out[i]` buffers,
    /// writing each length into `lens[i]`. `0` = none available (non-blocking).
    /// The default is one scalar [`recv`](Self::recv) into `out[0]`.
    fn recv_batch(&self, out: &mut [Vec<u8>], lens: &mut [usize]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        match self.recv()? {
            Some(pkt) => {
                let n = pkt.len().min(out[0].len());
                out[0][..n].copy_from_slice(&pkt[..n]);
                lens[0] = n;
                Ok(1)
            }
            None => Ok(0),
        }
    }
}
