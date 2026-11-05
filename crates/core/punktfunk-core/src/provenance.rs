//! Where a peer is: the address classes admission rules hang off, the header a page server
//! names its visitor in, and names that cannot be public.
//!
//! Pure. Asking the routing table is the caller's, injected into [`classify_with`], so this
//! builds for wasm and armv7.

use std::net::{IpAddr, Ipv4Addr};

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
    use std::net::Ipv6Addr;

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
