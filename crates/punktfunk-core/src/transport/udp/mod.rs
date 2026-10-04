//! Connected UDP datagram transport. Native sockets, no async runtime.
//!
//! [`UdpTransport`] implements [`Transport`]: send/recv never block; a full kernel
//! buffer or a connected-UDP ICMP blip is a lossy drop, never a teardown. Linux and
//! Android batch with `sendmmsg`/`recvmmsg` (Linux also UDP GSO); Windows uses USO;
//! Apple/BSD drain into reused buffers. Other targets keep the trait's scalar loop.
//!
//! Pin GSO with `PUNKTFUNK_GSO`; DSCP with `PUNKTFUNK_DSCP`. Platform bodies live in
//! `linux` / `windows` / `apple`.

use super::Transport;
use crate::packet::MAX_DATAGRAM_BYTES;
use std::net::UdpSocket;

// Emscripten is `unix` too, but has no `recvmsg_x` and no `libc::sockaddr_nl` — it takes the
// trait's scalar `recv_batch` instead.
#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_family = "wasm"))
))]
mod apple;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::send_uso_all;

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_family = "wasm"))
))]
#[cfg(feature = "quic")]
pub(crate) use apple::wait_readable;
/// Block until `socket` is readable or `timeout` passes; `true` when readable. The shared
/// socket's reader waits here, since quinn-udp keeps the socket non-blocking.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[cfg(feature = "quic")]
pub(crate) use linux::wait_readable;
#[cfg(target_os = "windows")]
#[cfg(feature = "quic")]
pub(crate) use windows::wait_readable;

/// One past [`MAX_DATAGRAM_BYTES`]. `Config::validate` keeps a well-formed datagram
/// (header + shard + crypto) inside that bound; a full read is oversized, not truncated.
const RECV_BUF: usize = MAX_DATAGRAM_BYTES + 1;

/// Lossy drop, not a stream teardown. `WouldBlock` is a full kernel buffer.
/// Connected-UDP `ConnectionRefused`/`ConnectionReset` are stale ICMP — a gone
/// peer is the QUIC control plane's timeout, not this socket. `ENOBUFS`,
/// `WSAENOBUFS` (10055), and the `ENET*`/`EHOST*` family have no stable
/// `ErrorKind` (Rust maps them to `Uncategorized`), so they are matched as
/// raw errno below — same contract as `WouldBlock`.
fn is_transient_io(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, ConnectionReset, WouldBlock};
    if matches!(e.kind(), WouldBlock | ConnectionRefused | ConnectionReset) {
        return true;
    }
    // No stable `ErrorKind` for these; match the raw errno.
    #[cfg(unix)]
    {
        matches!(
            e.raw_os_error(),
            Some(libc::ENOBUFS)
                | Some(libc::ENETUNREACH)
                | Some(libc::EHOSTUNREACH)
                | Some(libc::ENETDOWN)
                | Some(libc::EHOSTDOWN)
        )
    }
    // Winsock WSAE* raw codes (WSAEWOULDBLOCK already maps to WouldBlock).
    #[cfg(windows)]
    {
        matches!(
            e.raw_os_error(),
            Some(10055)   // WSAENOBUFS
                | Some(10051) // WSAENETUNREACH
                | Some(10065) // WSAEHOSTUNREACH
                | Some(10050) // WSAENETDOWN
                | Some(10064) // WSAEHOSTDOWN
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// The segment size of an offload batch: every packet `seg` bytes but the last, which may
/// be shorter. `None` for an empty or mixed batch, which goes out as plain datagrams.
#[cfg(any(target_os = "linux", target_os = "windows", test))]
fn uniform_segment(packets: &[&[u8]]) -> Option<usize> {
    let (last, rest) = packets.split_last()?;
    let seg = packets[0].len();
    (seg != 0 && rest.iter().all(|p| p.len() == seg) && last.len() <= seg).then_some(seg)
}

/// How a [`send_segmented`] run ended.
#[cfg(any(target_os = "linux", target_os = "windows", test))]
#[derive(Debug, PartialEq, Eq)]
enum Segmented {
    /// Packets sent. A transient error ends the run early; the caller owns the rest.
    Sent(usize),
    /// The path has no segment offload; this many packets went out before it said so.
    Unsupported(usize),
}

/// Coalesce a [`uniform_segment`] batch into `max_seg`-segment buffers and hand each to
/// `send_one` (GSO or USO) with the segment size. `unsupported` names the errors that
/// mean this path cannot offload; the caller latches that and picks the fallback.
#[cfg(any(target_os = "linux", target_os = "windows", test))]
fn send_segmented(
    packets: &[&[u8]],
    seg: usize,
    max_seg: usize,
    mut send_one: impl FnMut(&[u8], u16) -> std::io::Result<()>,
    unsupported: fn(&std::io::Error) -> bool,
) -> std::io::Result<Segmented> {
    let mut scratch: Vec<u8> = Vec::with_capacity(seg * packets.len().min(max_seg));
    let mut sent = 0usize;
    for chunk in packets.chunks(max_seg) {
        scratch.clear();
        for p in chunk {
            scratch.extend_from_slice(p);
        }
        match send_one(&scratch, seg as u16) {
            Ok(()) => sent += chunk.len(),
            Err(e) if is_transient_io(&e) => break,
            Err(e) if unsupported(&e) => return Ok(Segmented::Unsupported(sent)),
            Err(e) => return Err(e),
        }
    }
    Ok(Segmented::Sent(sent))
}

pub struct UdpTransport {
    /// qWAVE flow guard (Windows, opt-in DSCP): declared before `socket` so drop order removes
    /// the flow membership before the socket closes. Always `None` off-Windows.
    _qos_flow: Option<super::qos::QosFlow>,
    socket: UdpSocket,
    /// GSO asked for by the session (Linux), beside the process-wide env gate.
    gso: std::sync::atomic::AtomicBool,
}

impl UdpTransport {
    pub fn connect(local: &str, peer: &str) -> std::io::Result<Self> {
        Self::from_socket(UdpSocket::bind(local)?, peer)
    }

    /// Adopt an already-bound socket.
    pub fn from_socket(socket: UdpSocket, peer: &str) -> std::io::Result<Self> {
        socket.connect(peer)?;
        super::qos::grow_socket_buffers(&socket);
        // Video class (opt-in via PUNKTFUNK_DSCP). After `connect`: Windows qWAVE
        // requires a connected socket.
        let qos_flow = super::qos::set_media_qos(&socket, super::qos::MediaClass::Video);
        socket.set_nonblocking(true)?;
        Ok(UdpTransport {
            _qos_flow: qos_flow,
            socket,
            gso: std::sync::atomic::AtomicBool::new(false),
        })
    }

    #[cfg(target_os = "linux")]
    pub(super) fn gso_wanted(&self) -> bool {
        self.gso.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A clone of the socket while [`Session`](crate::Session) owns the transport.
    pub fn try_clone_socket(&self) -> std::io::Result<UdpSocket> {
        self.socket.try_clone()
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.socket.local_addr()
    }
}

impl Transport for UdpTransport {
    fn send(&self, packet: &[u8]) -> std::io::Result<bool> {
        match self.socket.send(packet) {
            Ok(_) => Ok(true),
            // Lossy drop (full tx queue / stale ICMP / path blip); `Ok(false)` is counted.
            Err(e) if is_transient_io(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn send_batch(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        linux::send_batch(self, packets)
    }

    #[cfg(target_os = "linux")]
    fn send_gso(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        linux::send_gso(self, packets)
    }

    fn set_gso(&self, on: bool) {
        self.gso.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(target_os = "windows")]
    fn send_gso(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        windows::send_gso(self, packets)
    }

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; RECV_BUF];
        match self.socket.recv(&mut buf) {
            // Full buffer = larger than any valid packet; drop rather than truncate.
            Ok(n) if n >= RECV_BUF => Ok(None),
            Ok(n) => {
                buf.truncate(n);
                Ok(Some(buf))
            }
            Err(e) if is_transient_io(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn recv_batch(&self, out: &mut [Vec<u8>], lens: &mut [usize]) -> std::io::Result<usize> {
        linux::recv_batch(self, out, lens)
    }

    #[cfg(all(
        unix,
        not(any(target_os = "linux", target_os = "android", target_family = "wasm"))
    ))]
    fn recv_batch(&self, out: &mut [Vec<u8>], lens: &mut [usize]) -> std::io::Result<usize> {
        apple::recv_batch(self, out, lens)
    }

    #[cfg(target_os = "windows")]
    fn recv_batch(&self, out: &mut [Vec<u8>], lens: &mut [usize]) -> std::io::Result<usize> {
        windows::recv_batch(self, out, lens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Transport;

    #[test]
    fn transient_io_covers_connected_udp_blips() {
        use std::io::{Error, ErrorKind};
        for k in [
            ErrorKind::WouldBlock,
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
        ] {
            assert!(
                is_transient_io(&Error::from(k)),
                "{k:?} should be transient"
            );
        }
        for k in [ErrorKind::PermissionDenied, ErrorKind::AddrInUse] {
            assert!(!is_transient_io(&Error::from(k)), "{k:?} must stay fatal");
        }
    }

    #[test]
    fn uniform_segment_takes_a_short_last_packet_only() {
        let (a, b, c) = ([0u8; 3], [0u8; 2], [0u8; 4]);
        assert_eq!(uniform_segment(&[&a, &a, &b]), Some(3));
        assert_eq!(uniform_segment(&[&a]), Some(3));
        assert_eq!(uniform_segment(&[]), None);
        assert_eq!(uniform_segment(&[&a, &c]), None);
        assert_eq!(uniform_segment(&[&a, &b, &a]), None);
        assert_eq!(uniform_segment(&[&[]]), None);
    }

    /// Five 3-byte packets at two segments per send: 6 + 6 + 3 bytes, and the error
    /// ladder stops at the failing call.
    #[test]
    fn send_segmented_chunks_then_stops_on_error() {
        use std::io::{Error, ErrorKind};
        let p = [0u8; 3];
        let packets: [&[u8]; 5] = [&p; 5];
        let unsupported = |e: &Error| e.kind() == ErrorKind::Unsupported;
        let run = |fail_at: usize, kind: ErrorKind| {
            let mut calls = Vec::new();
            let r = send_segmented(
                &packets,
                3,
                2,
                |buf, seg| {
                    calls.push((buf.len(), seg));
                    if calls.len() == fail_at {
                        return Err(Error::from(kind));
                    }
                    Ok(())
                },
                unsupported,
            );
            (r.map_err(|e| e.kind()), calls)
        };
        let (r, calls) = run(0, ErrorKind::Other);
        assert_eq!(r, Ok(Segmented::Sent(5)));
        assert_eq!(calls, [(6, 3), (6, 3), (3, 3)]);
        assert_eq!(run(2, ErrorKind::WouldBlock).0, Ok(Segmented::Sent(2)));
        assert_eq!(
            run(2, ErrorKind::Unsupported).0,
            Ok(Segmented::Unsupported(2))
        );
        assert_eq!(
            run(2, ErrorKind::PermissionDenied).0,
            Err(ErrorKind::PermissionDenied)
        );
    }

    /// Raw errno with no stable `ErrorKind` (they surface as `Uncategorized`).
    #[test]
    fn transient_io_covers_raw_tx_queue_and_path_codes() {
        use std::io::Error;

        #[cfg(unix)]
        {
            for code in [
                libc::ENOBUFS,
                libc::ENETUNREACH,
                libc::EHOSTUNREACH,
                libc::ENETDOWN,
                libc::EHOSTDOWN,
            ] {
                assert!(
                    is_transient_io(&Error::from_raw_os_error(code)),
                    "unix errno {code} should be transient"
                );
            }
            assert!(
                !is_transient_io(&Error::from_raw_os_error(libc::EACCES)),
                "EACCES must stay fatal"
            );
        }

        #[cfg(windows)]
        {
            // WSAENOBUFS / WSAENETUNREACH / WSAEHOSTUNREACH / WSAENETDOWN / WSAEHOSTDOWN.
            for code in [10055, 10051, 10065, 10050, 10064] {
                assert!(
                    is_transient_io(&Error::from_raw_os_error(code)),
                    "WSA code {code} should be transient"
                );
            }
            assert!(
                !is_transient_io(&Error::from_raw_os_error(10013)),
                "WSAEACCES must stay fatal"
            );
        }
    }

    /// 100 × 200 B = 20 KB, under the loopback socket buffer, so every packet must arrive.
    #[test]
    fn send_batch_delivers_over_loopback() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        let rx_addr = rx.local_addr().unwrap().to_string();
        let tx = UdpTransport::connect("127.0.0.1:0", &rx_addr).unwrap();

        const N: u32 = 100;
        let payloads: Vec<Vec<u8>> = (0..N)
            .map(|i| {
                let mut v = vec![0u8; 200];
                v[0..4].copy_from_slice(&i.to_le_bytes());
                v
            })
            .collect();
        let refs: Vec<&[u8]> = payloads.iter().map(|p| p.as_slice()).collect();
        let sent = tx.send_batch(&refs).unwrap();
        assert_eq!(
            sent, N as usize,
            "send_batch should hand all packets to the kernel"
        );

        let mut seen = std::collections::HashSet::new();
        let mut buf = [0u8; 2048];
        while seen.len() < N as usize {
            match rx.recv(&mut buf) {
                Ok(n) => {
                    assert_eq!(
                        n, 200,
                        "datagram boundaries preserved (one packet per recv)"
                    );
                    seen.insert(u32::from_le_bytes(buf[0..4].try_into().unwrap()));
                }
                Err(_) => break, // timeout: let the assert report the shortfall
            }
        }
        assert_eq!(
            seen.len(),
            N as usize,
            "every batched packet should arrive over loopback"
        );
    }

    #[test]
    fn recv_batch_drains_over_loopback() {
        // Transport under test is the receiver; a raw socket sends so the connected filter accepts it.
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let tx_addr = tx.local_addr().unwrap().to_string();
        let rx = UdpTransport::connect("127.0.0.1:0", &tx_addr).unwrap();
        let rx_addr = rx.local_addr().unwrap();

        const N: u32 = 50;
        for i in 0..N {
            let mut p = vec![0u8; 300];
            p[0..4].copy_from_slice(&i.to_le_bytes());
            tx.send_to(&p, rx_addr).unwrap();
        }

        let mut bufs: Vec<Vec<u8>> = (0..16).map(|_| vec![0u8; RECV_BUF]).collect();
        let mut lens = vec![0usize; 16];
        let mut seen = std::collections::HashSet::new();
        // A few drains absorb scheduling jitter; stop once all N are in or we go dry.
        for _ in 0..50 {
            let n = rx.recv_batch(&mut bufs, &mut lens).unwrap();
            if n == 0 {
                if seen.len() == N as usize {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            for i in 0..n {
                assert_eq!(lens[i], 300, "recvmmsg reports the datagram length");
                seen.insert(u32::from_le_bytes(bufs[i][0..4].try_into().unwrap()));
            }
        }
        assert_eq!(
            seen.len(),
            N as usize,
            "every datagram should be drained via recv_batch"
        );
    }
}
