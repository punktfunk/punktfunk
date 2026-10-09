//! Wake-on-LAN magic-packet builder and broadcast sender.
//!
//! Fire-and-forget UDP; no `quic` feature and no async runtime. Rust clients
//! call the `pub fn`s; Swift/iOS uses `punktfunk_wake_on_lan` in `punktfunk-ffi`.
//!
//! A sleeping host has no ARP entry, so unicast alone cannot wake it, and
//! `255.255.255.255` from an unbound socket follows only the default route
//! (often a VPN). For each MAC the 102-byte packet is sent:
//! - from every non-loopback IPv4 interface, bound to that NIC, to its
//!   subnet-directed broadcast and `255.255.255.255`;
//! - from an unbound socket to `255.255.255.255` and, when known, the last
//!   unicast IP;
//!
//! each on ports 9 and 7, [`BURST`] times.
//!
//! WoWLAN uses the same path: the AP buffers broadcast for sleeping stations
//! and flushes on DTIM, but only if egress is the wireless NIC. Host-side
//! arming is `punktfunk-host`'s `wol` module.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

pub type Mac = [u8; 6];

/// Conventional WoL UDP ports. 9 (discard) is the common one; 7 (echo) is a
/// historical alternative some NICs also listen on.
const WOL_PORTS: [u16; 2] = [9, 7];

/// Retransmits per call. UDP is lossy and this is fire-and-forget; a short burst
/// is cheap. The caller's connect-retry loop covers longer-spaced retries.
const BURST: usize = 3;

/// Parse `aa:bb:cc:dd:ee:ff` or `aa-bb-...`, case-insensitive, into 6 bytes.
/// `None` unless there are exactly six hex octets.
pub fn parse_mac(s: &str) -> Option<Mac> {
    let mut m = [0u8; 6];
    let mut n = 0;
    for part in s.split([':', '-']) {
        if n == 6 {
            return None;
        }
        m[n] = u8::from_str_radix(part.trim(), 16).ok()?;
        n += 1;
    }
    (n == 6).then_some(m)
}

pub fn build_magic_packet(mac: Mac) -> [u8; 102] {
    let mut pkt = [0xFFu8; 102];
    for i in 0..16 {
        let off = 6 + i * 6;
        pkt[off..off + 6].copy_from_slice(&mac);
    }
    pkt
}

/// Wake every MAC in `macs`, plus unicast `last_known_ip` when set.
///
/// `Ok` if at least one datagram left; one unreachable target does not fail the
/// wake. Errors only if no socket opened or nothing was sent.
pub fn send_magic_packet(macs: &[Mac], last_known_ip: Option<Ipv4Addr>) -> io::Result<()> {
    send_magic_packet_on(macs, last_known_ip, &WOL_PORTS)
}

/// [`send_magic_packet`] with explicit destination ports. Tests bind an unprivileged
/// port because 9 and 7 need root.
fn send_magic_packet_on(
    macs: &[Mac],
    last_known_ip: Option<Ipv4Addr>,
    ports: &[u16],
) -> io::Result<()> {
    if macs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no MAC addresses",
        ));
    }
    let packets: Vec<[u8; 102]> = macs.iter().map(|m| build_magic_packet(*m)).collect();

    // Default-route targets: limited broadcast, plus optional unicast (destination
    // routing picks the NIC, so unicast needs no per-interface treatment).
    let mut routed: Vec<Ipv4Addr> = vec![Ipv4Addr::BROADCAST];
    if let Some(ip) = last_known_ip {
        routed.push(ip);
    }

    let mut sent_any = false;

    // Bind each non-loopback IPv4 so the datagram leaves on that segment. Unbound
    // `255.255.255.255` follows only the default route (often a VPN), never the LAN
    // or the Wi-Fi segment a sleeping WoWLAN station is associated to.
    for (local, bcast) in local_v4_segments() {
        let Ok(sock) = UdpSocket::bind(SocketAddrV4::new(local, 0)) else {
            // Address gone or refused: still send this directed broadcast on the routed socket.
            routed.push(bcast);
            continue;
        };
        if sock.set_broadcast(true).is_err() {
            routed.push(bcast);
            continue;
        }
        sent_any |= blast(&sock, &packets, &[bcast, Ipv4Addr::BROADCAST], ports);
    }

    // Routed pass; the only pass when interfaces cannot be enumerated.
    if let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        // A refused SO_BROADCAST still lets unicast out; per-interface sockets may
        // already have carried the broadcast.
        let _ = sock.set_broadcast(true);
        routed.sort_unstable();
        routed.dedup();
        sent_any |= blast(&sock, &packets, &routed, ports);
    } else if !sent_any {
        return Err(io::Error::other("no socket could be opened for the wake"));
    }

    if sent_any {
        Ok(())
    } else {
        Err(io::Error::other("no magic packet could be sent"))
    }
}

/// An unroutable target never fails the wake.
fn blast(sock: &UdpSocket, packets: &[[u8; 102]], targets: &[Ipv4Addr], ports: &[u16]) -> bool {
    let mut sent_any = false;
    for _ in 0..BURST {
        for pkt in packets {
            for ip in targets {
                // 0.0.0.0 is not a destination.
                if ip.is_unspecified() {
                    continue;
                }
                for port in ports {
                    let dst = SocketAddr::V4(SocketAddrV4::new(*ip, *port));
                    if sock.send_to(pkt, dst).is_ok() {
                        sent_any = true;
                    }
                }
            }
        }
    }
    sent_any
}

/// Non-loopback IPv4 as `(address, subnet-directed broadcast)`. OS broadcast if
/// present, else `ip | !netmask`. Enumeration failure yields empty; the routed
/// pass still fires.
#[cfg(target_family = "wasm")]
fn local_v4_segments() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    Vec::new()
}

#[cfg(not(target_family = "wasm"))]
fn local_v4_segments() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    let mut out = Vec::new();
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(i) => i,
        Err(_) => return out,
    };
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            if v4.ip.is_unspecified() {
                continue;
            }
            let bcast = v4
                .broadcast
                .unwrap_or_else(|| Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask)));
            out.push((v4.ip, bcast));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet_layout() {
        let mac: Mac = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        let pkt = build_magic_packet(mac);
        assert_eq!(pkt.len(), 102);
        assert_eq!(&pkt[0..6], &[0xFF; 6]);
        for i in 0..16 {
            let off = 6 + i * 6;
            assert_eq!(&pkt[off..off + 6], &mac, "repetition {i} mismatch");
        }
    }

    #[test]
    fn empty_macs_is_error() {
        assert!(send_magic_packet(&[], None).is_err());
    }

    #[test]
    fn parse_mac_forms() {
        assert_eq!(
            parse_mac("aa:bb:cc:dd:ee:ff"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        assert_eq!(
            parse_mac("AA-BB-CC-DD-EE-FF"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        assert_eq!(parse_mac("01:02:03:04:05:06"), Some([1, 2, 3, 4, 5, 6]));
        assert_eq!(parse_mac("aa:bb:cc:dd:ee"), None);
        assert_eq!(parse_mac("aa:bb:cc:dd:ee:ff:00"), None);
        assert_eq!(parse_mac("zz:bb:cc:dd:ee:ff"), None);
        assert_eq!(parse_mac(""), None);
    }

    #[test]
    fn send_does_not_panic_with_a_mac() {
        let _ = send_magic_packet(&[[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]], None);
    }

    #[test]
    fn local_segments_are_bindable_and_have_a_broadcast() {
        for (local, bcast) in local_v4_segments() {
            assert!(!local.is_unspecified());
            assert!(!local.is_loopback());
            assert!(!bcast.is_unspecified());
            assert!(UdpSocket::bind(SocketAddrV4::new(local, 0)).is_ok());
        }
    }

    #[test]
    fn blast_reports_nothing_sent_for_an_empty_target_list() {
        let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
        let pkt = [build_magic_packet([1, 2, 3, 4, 5, 6])];
        assert!(!blast(&sock, &pkt, &[], &WOL_PORTS));
        assert!(!blast(&sock, &pkt, &[Ipv4Addr::UNSPECIFIED], &WOL_PORTS));
        // Loopback is a real destination; this send must succeed.
        assert!(blast(&sock, &pkt, &[Ipv4Addr::LOCALHOST], &[9999]));
    }

    /// End-to-end: a listener on loopback gets the right 102 bytes. Uses an
    /// unprivileged port (9 and 7 need root). Exercises the routed unicast leg —
    /// the path that still wakes a WoWLAN host when the AP filters broadcast.
    #[test]
    fn send_delivers_the_magic_packet_to_a_listener() {
        let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind receiver");
        let port = rx.local_addr().expect("local addr").port();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("read timeout");

        let mac: Mac = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        send_magic_packet_on(&[mac], Some(Ipv4Addr::LOCALHOST), &[port]).expect("send");

        let mut buf = [0u8; 256];
        let (n, _from) = rx.recv_from(&mut buf).expect("a magic packet must arrive");
        assert_eq!(n, 102);
        assert_eq!(buf[..102], build_magic_packet(mac));
    }
}
