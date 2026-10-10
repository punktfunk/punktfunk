//! Shared UDP socket tuning: `SO_SNDBUF`/`SO_RCVBUF` growth and best-effort DSCP.
//!
//! [`grow_socket_buffers`] is what the native data plane and GameStream sockets
//! apply so a keyframe burst does not ENOBUFS.
//!
//! [`set_media_qos`] tags video CS5 / audio CS6 (Linux also `SO_PRIORITY`) so a
//! WMM AP or managed switch can prefer media over bulk. Default: on toward
//! RFC1918 / ULA / link-local / loopback, off toward anything routable — WAN
//! paths bleach or reject DSCP. `PUNKTFUNK_DSCP=1` forces on, `=0` kills it;
//! [`set_dscp_default`] still forces on (Android low-latency). Windows strips
//! a plain `IP_TOS`, so the mark goes through qWAVE ([`super::qos_windows`]);
//! hold the returned [`QosFlow`] while the socket sends media.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};

/// Target `SO_SNDBUF`/`SO_RCVBUF`. Default UDP buffers (~208 KB on Linux) overflow
/// a multi-MB IDR burst and freeze decode until the next refresh. 32 MB ≈ 200 ms
/// at 1 Gbps; paced send (`native.rs::paced_submit`) spreads the rest. The OS
/// clamps to `net.core.{wmem,rmem}_max` / `kern.ipc.maxsockbuf`.
pub(crate) const TARGET_SOCKBUF: usize = 32 * 1024 * 1024;

/// Best-effort grow of `SO_SNDBUF`/`SO_RCVBUF` to [`TARGET_SOCKBUF`]. Failure is
/// not fatal; a grant far below the request means the OS cap is too low, so warn
/// with both sizes and the sysctl lines that lift it: send and receive are capped by
/// separate limits.
pub fn grow_socket_buffers(socket: &UdpSocket) {
    let sock = socket2::SockRef::from(socket);
    let _ = sock.set_send_buffer_size(TARGET_SOCKBUF);
    let _ = sock.set_recv_buffer_size(TARGET_SOCKBUF);
    let send = sock.send_buffer_size().unwrap_or(0);
    let recv = sock.recv_buffer_size().unwrap_or(0);
    if send.min(recv) < TARGET_SOCKBUF / 4 {
        tracing::warn!(
            send_kb = send / 1024,
            recv_kb = recv / 1024,
            linux_fix = %format_args!(
                "net.core.rmem_max = {TARGET_SOCKBUF}, net.core.wmem_max = {TARGET_SOCKBUF} in /etc/sysctl.d"
            ),
            "UDP socket buffer capped well below target; 4K and 5K streams may drop frames"
        );
    }
}

/// Selects DSCP (and Linux `SO_PRIORITY`): video CS5, audio CS6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaClass {
    Video,
    Audio,
}

impl MediaClass {
    /// DSCP code point (high 6 bits of the IPv4 TOS / IPv6 traffic-class byte).
    pub(super) const fn dscp(self) -> u32 {
        match self {
            MediaClass::Video => 40, // CS5
            MediaClass::Audio => 48, // CS6
        }
    }
}

/// Embedder force-on when `PUNKTFUNK_DSCP` is unset. `false` is AUTO (private
/// peers only); `true` marks every peer (e.g. Android low-latency over a VPN
/// the address math cannot see as local).
static DSCP_DEFAULT: AtomicBool = AtomicBool::new(false);

/// Force DSCP on for sockets created from now on (`false` restores AUTO). Call
/// before connect — the tag is applied at socket creation. `PUNKTFUNK_DSCP`
/// still overrides.
pub fn set_dscp_default(enabled: bool) {
    DSCP_DEFAULT.store(enabled, Ordering::Relaxed);
}

/// Env wins both ways (`1`/`true`/`on` force, `0`/`false`/`off` kill); else the
/// embedder force-on; else AUTO — mark only a private-network peer.
fn dscp_decision(env: Option<&str>, embedder_on: bool, peer_private: bool) -> bool {
    match env {
        Some("1") | Some("true") | Some("on") => true,
        Some("0") | Some("false") | Some("off") => false,
        _ => embedder_on || peer_private,
    }
}

pub(crate) fn dscp_enabled_for(peer: Option<std::net::SocketAddr>) -> bool {
    dscp_decision(
        std::env::var("PUNKTFUNK_DSCP").ok().as_deref(),
        DSCP_DEFAULT.load(Ordering::Relaxed),
        peer.is_some_and(|p| is_private_peer(&p)),
    )
}

/// RFC1918 / link-local / loopback IPv4, and loopback / ULA (`fc00::/7`) /
/// link-local (`fe80::/10`) IPv6, including v4-mapped. DSCP bleach lives on
/// ISP paths — none of these cross one.
fn is_private_peer(addr: &std::net::SocketAddr) -> bool {
    fn v4(ip: std::net::Ipv4Addr) -> bool {
        ip.is_private() || ip.is_loopback() || ip.is_link_local()
    }
    match addr.ip() {
        std::net::IpAddr::V4(ip) => v4(ip),
        std::net::IpAddr::V6(ip) => {
            let seg0 = ip.segments()[0];
            ip.is_loopback()
                || (seg0 & 0xfe00) == 0xfc00
                || (seg0 & 0xffc0) == 0xfe80
                || ip.to_ipv4_mapped().is_some_and(v4)
        }
    }
}

/// RAII token for a socket's QoS mark. On Windows it is qWAVE flow membership
/// ([`super::qos_windows::QosFlow`]) — drop removes the mark, so hold it while
/// the socket sends. Elsewhere the mark is a socket option and this is inert
/// ([`set_media_qos`] returns `None`).
#[cfg(windows)]
pub use super::qos_windows::QosFlow;
#[cfg(not(windows))]
pub struct QosFlow {
    _never: std::convert::Infallible,
}

/// Best-effort DSCP for `class`. No-op toward a non-private peer unless forced.
/// Failures log at debug, never fatal. Socket must already be `connect`ed
/// (qWAVE and the private-peer check both need the 5-tuple). IPv4 only.
/// Returns [`QosFlow`] on Windows — keep it with the socket; `None` elsewhere.
pub fn set_media_qos(socket: &UdpSocket, class: MediaClass) -> Option<QosFlow> {
    if !dscp_enabled_for(socket.peer_addr().ok()) {
        return None;
    }
    #[cfg(windows)]
    {
        super::qos_windows::add_media_flow(socket, class)
    }
    #[cfg(not(windows))]
    {
        apply_media_qos(socket, class);
        None
    }
}

/// Unconditional QoS apply, split out of [`set_media_qos`] so tests need not
/// touch `PUNKTFUNK_DSCP`. Best-effort (log and continue).
#[cfg_attr(windows, allow(dead_code))]
fn apply_media_qos(socket: &UdpSocket, class: MediaClass) {
    let sock = socket2::SockRef::from(socket);
    // DSCP occupies the high 6 bits of the TOS byte → shift left 2.
    if let Err(e) = sock.set_tos_v4(class.dscp() << 2) {
        tracing::debug!(error = %e, ?class, "set IP_TOS (DSCP) failed — QoS marking skipped");
    }
    // SO_PRIORITY after IP_TOS (TOS resets it to 0 on Linux). 6 is the max without
    // CAP_NET_ADMIN, so video=5 / audio=6.
    #[cfg(target_os = "linux")]
    {
        let prio = match class {
            MediaClass::Video => 5,
            MediaClass::Audio => 6,
        };
        if let Err(e) = sock.set_priority(prio) {
            tracing::debug!(error = %e, "set SO_PRIORITY failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dscp_code_points_match_apollo() {
        assert_eq!(MediaClass::Video.dscp(), 40);
        assert_eq!(MediaClass::Audio.dscp(), 48);
        assert_eq!(MediaClass::Video.dscp() << 2, 0xA0);
        assert_eq!(MediaClass::Audio.dscp() << 2, 0xC0);
    }

    #[test]
    fn qos_and_buffer_growth_are_best_effort_and_never_panic() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        // Unconnected: no peer, AUTO stays off. Must not panic.
        assert!(set_media_qos(&sock, MediaClass::Video).is_none());
        assert!(set_media_qos(&sock, MediaClass::Audio).is_none());
        grow_socket_buffers(&sock);
    }

    /// Default marks only local-network peers; env still wins both ways and the
    /// embedder hook still forces on (VPN path the address math misses).
    #[test]
    fn dscp_defaults_on_for_private_peers_only() {
        assert!(dscp_decision(Some("1"), false, false));
        assert!(
            !dscp_decision(Some("0"), true, true),
            "the kill switch beats everything"
        );
        assert!(dscp_decision(None, true, false), "embedder force-on");
        assert!(
            dscp_decision(None, false, true),
            "AUTO: a private peer marks"
        );
        assert!(
            !dscp_decision(None, false, false),
            "AUTO: a routable peer does not"
        );

        use std::net::SocketAddr;
        for a in [
            "192.168.1.20:47999",
            "10.0.0.7:1",
            "172.16.3.4:5",
            "169.254.10.1:2",
            "127.0.0.1:9",
            "[fe80::1]:1",
            "[fd12:3456::1]:1",
            "[::1]:1",
            "[::ffff:192.168.1.2]:1",
        ] {
            assert!(
                is_private_peer(&a.parse::<SocketAddr>().unwrap()),
                "{a} is local"
            );
        }
        for a in [
            "1.1.1.1:53",
            "84.23.10.9:47999",
            "172.32.0.1:1", // one past RFC1918's 172.16/12
            "[2001:db8::1]:1",
            "[::ffff:8.8.8.8]:1",
        ] {
            assert!(
                !is_private_peer(&a.parse::<SocketAddr>().unwrap()),
                "{a} is routable"
            );
        }
    }

    /// A connected loopback socket has a private peer, so AUTO marks it.
    #[test]
    fn a_connected_loopback_socket_marks_by_default() {
        let target = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.connect(target.local_addr().unwrap()).unwrap();
        let _ = set_media_qos(&sock, MediaClass::Video);
        #[cfg(target_os = "linux")]
        {
            let s = socket2::SockRef::from(&sock);
            assert_eq!(s.tos_v4().unwrap(), 0xA0, "AUTO marked the private peer");
        }
    }

    #[test]
    fn apply_qos_tags_the_socket() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        apply_media_qos(&sock, MediaClass::Video);
        #[cfg(target_os = "linux")]
        {
            let s = socket2::SockRef::from(&sock);
            assert_eq!(s.tos_v4().unwrap(), 0xA0, "video → CS5 in the TOS byte");
            assert_eq!(s.priority().unwrap(), 5, "video → SO_PRIORITY 5");
        }
    }
}
