//! Connected UDP datagram transport. Native sockets, no async runtime.
//!
//! [`UdpTransport`] implements [`Transport`]: send/recv never block; a full kernel
//! buffer or a connected-UDP ICMP blip is a lossy drop, never a teardown. Linux and
//! Android batch with `sendmmsg`/`recvmmsg` (Linux also UDP GSO); Windows uses USO;
//! Apple/BSD drain into reused buffers. Other targets keep the trait's scalar loop.
//!
//! Hole-punch (`PUNCH_MAGIC`) opens the NAT/firewall return path so host→client
//! video follows the observed source. Pin GSO with `PUNKTFUNK_GSO`; DSCP with
//! `PUNKTFUNK_DSCP`. Platform bodies live in `linux` / `windows` / `apple`.

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

/// Client→host marker on the video data socket. Opens the NAT/firewall return
/// path and advertises the observed source. Never treated as stream data.
pub const PUNCH_MAGIC: &[u8] = b"PFpunch1";

/// Client-side punch keepalive on a clone of the connected data socket.
/// Stops when `stop` is set or the socket closes.
pub fn spawn_data_punch(sock: UdpSocket, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    std::thread::Builder::new()
        .name("punktfunk-data-punch".into())
        .spawn(move || {
            let mut i = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match sock.send(PUNCH_MAGIC) {
                    Ok(_) => {}
                    // Transient: keep the keepalive alive. Breaking here is silent
                    // and permanent — the mapping then expires during a static scene.
                    Err(e) if is_transient_io(&e) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "data-plane punch send failed — stopping keepalive");
                        break;
                    }
                }
                // 15 × 200 ms ≈ 3 s of bursts (host punch-wait ~2.5 s); then 2 s keepalive.
                let delay_ms = if i < 15 { 200 } else { 2000 };
                i = i.saturating_add(1);
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
        })
        .ok();
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

    /// Adopt an already-bound socket. The host binds the data port before
    /// handshake so a concurrent session cannot steal a fixed `--data-port`.
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

    /// Wait up to `punch_timeout` for [`PUNCH_MAGIC`] from `expect_ip`, then
    /// `connect` to the observed source so video returns through the path the
    /// client opened. No punch → `fallback_peer` (flat-LAN, same as [`connect`](Self::connect)).
    /// Returns `(transport, punched)`.
    pub fn connect_via_punch(
        local: &str,
        fallback_peer: &str,
        expect_ip: std::net::IpAddr,
        punch_timeout: std::time::Duration,
    ) -> std::io::Result<(Self, bool)> {
        Self::from_socket_punch(
            UdpSocket::bind(local)?,
            fallback_peer,
            expect_ip,
            punch_timeout,
        )
    }

    /// [`connect_via_punch`](Self::connect_via_punch) on an already-bound socket.
    ///
    /// `expect_ip` is the QUIC-authenticated peer. `PUNCH_MAGIC` carries no session
    /// id, so only that IP's punch is adopted — the port is what NAT remaps, and
    /// the client uses the same source IP on both planes.
    pub fn from_socket_punch(
        socket: UdpSocket,
        fallback_peer: &str,
        expect_ip: std::net::IpAddr,
        punch_timeout: std::time::Duration,
    ) -> std::io::Result<(Self, bool)> {
        let deadline = std::time::Instant::now() + punch_timeout;
        let mut buf = [0u8; 64];
        let mut observed: Option<std::net::SocketAddr> = None;
        loop {
            // Remaining budget, not a fresh full window: a stray flood must not
            // stretch the wait past `punch_timeout`.
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            socket.set_read_timeout(Some(remaining))?;
            match socket.recv_from(&mut buf) {
                Ok((n, src))
                    if src.ip() == expect_ip
                        && n >= PUNCH_MAGIC.len()
                        && &buf[..PUNCH_MAGIC.len()] == PUNCH_MAGIC =>
                {
                    observed = Some(src);
                    break;
                }
                // Off-peer or not PUNCH_MAGIC: keep waiting.
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break
                }
                Err(e) => return Err(e),
            }
        }
        let punched = observed.is_some();
        let target = observed.map(|s| s.to_string());
        socket.connect(target.as_deref().unwrap_or(fallback_peer))?;
        socket.set_read_timeout(None)?;
        super::qos::grow_socket_buffers(&socket);
        let qos_flow = super::qos::set_media_qos(&socket, super::qos::MediaClass::Video);
        socket.set_nonblocking(true)?;
        Ok((
            UdpTransport {
                _qos_flow: qos_flow,
                socket,
                gso: std::sync::atomic::AtomicBool::new(false),
            },
            punched,
        ))
    }

    /// Clone for punch keepalives while [`Session`](crate::Session) owns the transport.
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

    #[test]
    fn punch_adopts_remapped_port_from_the_authenticated_peer() {
        // Post-NAT data socket: same IP as the QUIC peer, new port.
        let puncher = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        puncher
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        // Client-reported address; video must not go here once the punch is adopted.
        let reported = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        reported
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();

        let host_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let host_addr = host_sock.local_addr().unwrap();
        puncher.send_to(PUNCH_MAGIC, host_addr).unwrap();

        let (transport, punched) = UdpTransport::from_socket_punch(
            host_sock,
            &reported.local_addr().unwrap().to_string(),
            std::net::IpAddr::from([127, 0, 0, 1]),
            std::time::Duration::from_millis(500),
        )
        .unwrap();
        assert!(punched, "a punch from the authenticated IP must be adopted");

        transport.send(b"video").unwrap();
        let mut buf = [0u8; 64];
        let n = puncher
            .recv(&mut buf)
            .expect("video must follow the punched (NAT-remapped) port");
        assert_eq!(&buf[..n], b"video");
        assert!(
            reported.recv(&mut buf).is_err(),
            "video must not go to the stale reported port"
        );
    }

    /// `connect` to the observed source binds the 5-tuple; later punches from the same IP must not land.
    #[test]
    fn a_punched_transport_only_accepts_the_punched_five_tuple() {
        let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let reported = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();

        let host_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let host_addr = host_sock.local_addr().unwrap();
        peer.send_to(PUNCH_MAGIC, host_addr).unwrap();

        let (transport, punched) = UdpTransport::from_socket_punch(
            host_sock,
            &reported.local_addr().unwrap().to_string(),
            std::net::IpAddr::from([127, 0, 0, 1]),
            std::time::Duration::from_millis(500),
        )
        .unwrap();
        assert!(punched, "the peer's punch must be adopted");

        // Same IP, different port, after the tuple is fixed. Neither datagram may be seen.
        let stray = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        stray.send_to(PUNCH_MAGIC, host_addr).unwrap();
        stray.send_to(b"stray", host_addr).unwrap();
        peer.send_to(b"real", host_addr).unwrap();

        let mut got: Vec<Vec<u8>> = Vec::new();
        for _ in 0..20 {
            match transport.recv().unwrap() {
                Some(p) => got.push(p),
                None => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
        assert_eq!(
            got,
            vec![b"real".to_vec()],
            "only the punched 5-tuple may reach the session"
        );
    }

    #[test]
    fn punch_from_an_unauthenticated_source_is_ignored() {
        let attacker = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        attacker
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        let legit = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        legit
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();

        let host_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let host_addr = host_sock.local_addr().unwrap();
        attacker.send_to(PUNCH_MAGIC, host_addr).unwrap();

        // TEST-NET-1: nothing arriving over loopback is this peer.
        let (transport, punched) = UdpTransport::from_socket_punch(
            host_sock,
            &legit.local_addr().unwrap().to_string(),
            std::net::IpAddr::from([192, 0, 2, 1]),
            std::time::Duration::from_millis(300),
        )
        .unwrap();
        assert!(
            !punched,
            "an off-peer punch must not be adopted as the video destination"
        );

        transport.send(b"video").unwrap();
        let mut buf = [0u8; 64];
        assert!(
            attacker.recv(&mut buf).is_err(),
            "video must never be redirected to the punch source"
        );
        let n = legit
            .recv(&mut buf)
            .expect("video falls back to the reported peer address");
        assert_eq!(&buf[..n], b"video");
    }
}
