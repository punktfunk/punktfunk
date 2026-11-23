//! Where a peer is: the address classes admission rules hang off, the header a page server
//! names its visitor in, and names that cannot be public.
//!
//! Pure. Asking the routing table is the caller's, injected into [`classify_with`], so this
//! builds for wasm and armv7.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The header a page server puts on a request or session it relays: the visitor's address, or
/// [`CLIENT_ADDR_UNKNOWN`].
pub const CLIENT_ADDR_HEADER: &str = "pf-client-addr";

/// [`CLIENT_ADDR_HEADER`]'s value when the page server cannot tell who the visitor is.
pub const CLIENT_ADDR_UNKNOWN: &str = "unknown";

/// Where a knock came from. The host cannot tell a stranger on the internet from a friend on
/// the couch by name — the name is whatever the device sent — so admission rules hang off the
/// address instead. Ordered by suspicion, so the stricter of two is their `max`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum KnockSource {
    /// Loopback, RFC 1918, link-local, IPv6 unique-local, or a Tailscale or NetBird network.
    /// Reaching any of these means the peer is already on a network the operator let it onto.
    Lan,
    /// Routable from the internet, or an address we could not read. Fails closed: every rule
    /// that treats a source as untrusted must land here rather than on a guess.
    Wan,
}

/// Classify a peer's address. `None` is [`KnockSource::Wan`]: an admission rule reading an
/// unknown address must refuse, not admit. `over_mesh` is asked only about a 100.64/10 peer:
/// that range is carrier-grade NAT space too, so the address alone proves nothing.
pub fn classify_with(ip: Option<IpAddr>, over_mesh: impl Fn(Ipv4Addr) -> bool) -> KnockSource {
    let Some(ip) = ip else {
        return KnockSource::Wan;
    };
    let v4 = match ip {
        IpAddr::V4(v4) => Some(v4),
        // A v4-mapped v6 source is the v4 address wearing a hat; classify what it really is.
        IpAddr::V6(v6) => {
            let s = v6.segments();
            // Loopback (::1), link-local fe80::/10, unique local fc00::/7.
            if v6.is_loopback() || (s[0] & 0xffc0) == 0xfe80 || (s[0] & 0xfe00) == 0xfc00 {
                return KnockSource::Lan;
            }
            v6.to_ipv4_mapped()
        }
    };
    match v4 {
        Some(v4)
            if v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (is_cgnat(v4) && over_mesh(v4)) =>
        {
            KnockSource::Lan
        }
        _ => KnockSource::Wan,
    }
}

/// IPv6 global unicast, 2000::/3: the only IPv6 range the public internet routes.
pub fn is_public_v6(ip: Ipv6Addr) -> bool {
    ip.segments()[0] & 0xe000 == 0x2000
}

/// Whether an IPv6 connection from `source` to our address `local` kept off the public
/// internet: both ends are unicast outside 2000::/3. The internet delivers to a public address
/// only, so `local` decides even when `source` lies. An address we could not read fails closed.
pub fn off_internet_v6(source: IpAddr, local: Option<IpAddr>) -> bool {
    let private = |ip: IpAddr| match ip {
        IpAddr::V6(v6) => {
            !is_public_v6(v6)
                && !v6.is_unspecified()
                && !v6.is_multicast()
                && v6.to_ipv4_mapped().is_none()
        }
        IpAddr::V4(_) => false,
    };
    private(source) && local.is_some_and(private)
}

/// The address a per-peer limit counts by. A public IPv6 subscriber holds a /64 at least, so
/// its /64 is the peer. Any other address is one device.
pub fn limit_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) if is_public_v6(v6) => {
            let s = v6.segments();
            Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0).into()
        }
        ip => ip,
    }
}

/// RFC 6598 shared space, 100.64/10: a mesh VPN's addresses and a CGNAT ISP's subscribers alike.
pub fn is_cgnat(v4: Ipv4Addr) -> bool {
    v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])
}

/// A mesh VPN's interface: Tailscale's `tailscale0` on Linux and `Tailscale` on Windows, NetBird's
/// `wt0` on both, a `utun` on macOS. No ISP puts a 100.64/10 address on any of those names.
pub fn is_mesh_iface(name: &str, addr: IpAddr) -> bool {
    let name = name.to_ascii_lowercase();
    (name.starts_with("tailscale") || name.starts_with("utun") || name == "wt0")
        && matches!(addr, IpAddr::V4(v4) if is_cgnat(v4))
}

/// A host name that cannot be a public DNS name: `localhost`, a single label, or one under
/// `.local`, `.home.arpa` or `.internal`. Case-insensitive; a trailing dot is tolerated.
///
/// Fails closed: an IP literal, a port, an empty label or a character outside letters, digits
/// and `-` is not local. A single label must start with a letter, because a browser reads
/// `134744072` or `0x8080808` as an IPv4 address.
pub fn is_local_name(host: &str) -> bool {
    let name = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let well_formed = name
        .split('.')
        .all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    let single = !name.contains('.') && name.starts_with(|c: char| c.is_ascii_alphabetic());
    well_formed
        && (single
            || [".local", ".home.arpa", ".internal"]
                .iter()
                .any(|s| name.ends_with(s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knock_sources_classify_by_address() {
        let classify = |ip| classify_with(ip, |_| false);
        let lan: [IpAddr; 6] = [
            Ipv4Addr::LOCALHOST.into(),
            Ipv4Addr::new(10, 1, 2, 3).into(),
            Ipv4Addr::new(172, 16, 0, 1).into(),
            Ipv4Addr::new(192, 168, 1, 44).into(),
            Ipv4Addr::new(169, 254, 3, 4).into(),
            Ipv6Addr::LOCALHOST.into(),
        ];
        for ip in lan {
            assert_eq!(classify(Some(ip)), KnockSource::Lan, "{ip}");
        }
        let wan: [IpAddr; 6] = [
            Ipv4Addr::new(203, 0, 113, 5).into(),
            Ipv4Addr::new(8, 8, 8, 8).into(),
            // 172.32/12 is outside RFC 1918; 100.128/9 is outside the CGNAT block.
            Ipv4Addr::new(172, 32, 0, 1).into(),
            Ipv4Addr::new(100, 128, 0, 1).into(),
            Ipv4Addr::UNSPECIFIED.into(),
            "2606:4700::1111".parse::<IpAddr>().unwrap(),
        ];
        for ip in wan {
            assert_eq!(classify(Some(ip)), KnockSource::Wan, "{ip}");
        }
        assert_eq!(
            classify(None),
            KnockSource::Wan,
            "an unknown source fails closed"
        );
        // A v4 address wearing a v6 hat is classified as what it really is.
        assert_eq!(
            classify(Some("::ffff:192.168.1.44".parse().unwrap())),
            KnockSource::Lan
        );
        assert_eq!(
            classify(Some("::ffff:203.0.113.5".parse().unwrap())),
            KnockSource::Wan
        );
        // fc00::/7 unique-local and fe80::/10 link-local are both on-link.
        assert_eq!(classify(Some("fd00::1".parse().unwrap())), KnockSource::Lan);
        assert_eq!(classify(Some("fe80::1".parse().unwrap())), KnockSource::Lan);
    }

    /// Only a connection with both ends outside 2000::/3 kept off the internet.
    #[test]
    fn a_public_end_puts_an_ipv6_connection_on_the_internet() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let ours = Some(ip("fd00::10"));
        // Unique-local, link-local, loopback and Yggdrasil's 0200::/7.
        for source in ["fd7a:115c:a1e0::5", "fe80::1", "::1", "200:1234::1"] {
            assert!(off_internet_v6(ip(source), ours), "{source}");
        }
        // Global unicast, 6to4, Teredo, and sources no peer sends from.
        for source in [
            "2606:4700::1111",
            "2002:c000:204::1",
            "2001:0:4136:e378::1",
            "3fff::1",
            "::",
            "ff02::1",
            "::ffff:192.168.1.9",
            "192.168.1.9",
        ] {
            assert!(!off_internet_v6(ip(source), ours), "{source}");
        }
        // A private source that reached a public address of ours came over the internet.
        assert!(!off_internet_v6(ip("fd00::9"), Some(ip("2a02:8070::10"))));
        assert!(!off_internet_v6(ip("fd00::9"), None), "fails closed");
    }

    /// A public IPv6 peer is counted by its /64; every other address is itself.
    #[test]
    fn a_limit_counts_a_public_ipv6_peer_by_its_64() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert_eq!(
            limit_key(ip("2a02:8070:1:2:aaaa::1")),
            ip("2a02:8070:1:2::")
        );
        assert_eq!(
            limit_key(ip("2a02:8070:1:2:aaaa::1")),
            limit_key(ip("2a02:8070:1:2:bbbb::2"))
        );
        assert_ne!(
            limit_key(ip("2a02:8070:1:2::1")),
            limit_key(ip("2a02:8070:1:3::1"))
        );
        for one_device in ["192.168.1.9", "fd00::9", "fe80::9", "200:1234::9"] {
            assert_eq!(limit_key(ip(one_device)), ip(one_device), "{one_device}");
        }
        assert_eq!(limit_key(ip("::ffff:192.168.1.9")), ip("192.168.1.9"));
    }

    /// 100.64/10 is a mesh VPN's range and a CGNAT ISP's alike, so the route back decides.
    #[test]
    fn a_cgnat_peer_is_lan_only_over_a_mesh_vpn() {
        let peer: IpAddr = "100.96.0.7".parse().unwrap();
        assert_eq!(classify_with(Some(peer), |_| true), KnockSource::Lan);
        // The CGNAT neighbour: the same range, answered out of the ordinary uplink.
        assert_eq!(classify_with(Some(peer), |_| false), KnockSource::Wan);
        let mapped = "::ffff:100.96.0.7".parse().ok();
        assert_eq!(classify_with(mapped, |_| false), KnockSource::Wan);
        // The mesh is asked about 100.64/10 only.
        for ip in ["203.0.113.5", "100.128.0.1"] {
            assert_eq!(classify_with(ip.parse().ok(), |_| true), KnockSource::Wan);
        }

        for name in ["tailscale0", "Tailscale", "utun4", "wt0"] {
            assert!(is_mesh_iface(name, peer), "{name}");
        }
        // An ISP puts its CGNAT address on the uplink, and only NetBird's default name counts.
        for name in [
            "eth0", "en0", "ppp0", "Ethernet", "wlan0", "wg0", "wt1", "wt0x",
        ] {
            assert!(!is_mesh_iface(name, peer), "{name}");
        }
        // The name alone proves nothing: the address has to be 100.64/10 too.
        for name in ["utun4", "wt0"] {
            assert!(!is_mesh_iface(name, "10.8.0.2".parse().unwrap()), "{name}");
        }
    }

    #[test]
    fn local_names_are_the_ones_no_public_zone_can_hold() {
        for name in [
            "localhost",
            "LOCALHOST.",
            "gaming-pc",
            "nas.local",
            "Desk.Local.",
            "box.home.arpa",
            "host.lan.internal",
        ] {
            assert!(is_local_name(name), "{name}");
        }
        for name in [
            "",
            ".",
            "web.punktfunk.io",
            "evil.example",
            "local.evil.example",
            "evil.notinternal",
            "192.168.1.10",
            "::1",
            "[::1]",
            "localhost:8080",
            "134744072",
            "0x8080808",
            "a..local",
            "nas.local..",
            "under_score.local",
        ] {
            assert!(!is_local_name(name), "{name}");
        }
    }
}
