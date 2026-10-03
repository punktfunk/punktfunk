//! Which A record to dial when an mDNS advert resolves to several.
//!
//! The resolved set is a union of answers from every responder on every
//! interface. The host advert registers one address (its routed primary;
//! host crate `discovery.rs`), but the OS mDNS responder also answers
//! `<host>.local.` per interface. Overlay networks add their addresses
//! to the same set.
//!
//! [`rank_host_addr`] is the pure policy; [`pick_host_addr`] applies it
//! with this machine's live context. [`advert_from_txt`] turns one resolved
//! advert into the host record every client's browse loop keeps.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

/// Shared leading bits — the ranking's on-link proxy. No netmasks: a longer
/// prefix with one of our addresses is "more on this segment".
fn prefix_bits(a: Ipv4Addr, b: Ipv4Addr) -> u32 {
    (u32::from(a) ^ u32::from(b)).leading_zeros()
}

/// Address to dial, chosen deterministically. Best score wins:
///
/// 1. longest common prefix with any of this machine's unicast addresses
///    (on-link beats routed; overlay wins only when we have no LAN match);
/// 2. the address the host declared (mDNS TXT `addr`);
/// 3. longest common prefix with our default-route source;
/// 4. numerically lowest — so a re-announce cannot flap the pick.
pub fn rank_host_addr(
    candidates: &[Ipv4Addr],
    host_declared: Option<Ipv4Addr>,
    local_ips: &[Ipv4Addr],
    routed_local: Option<Ipv4Addr>,
) -> Option<Ipv4Addr> {
    candidates.iter().copied().max_by_key(|&c| {
        (
            local_ips
                .iter()
                .map(|&l| prefix_bits(c, l))
                .max()
                .unwrap_or(0),
            host_declared == Some(c),
            routed_local.map_or(0, |r| prefix_bits(c, r)),
            std::cmp::Reverse(u32::from(c)),
        )
    })
}

/// [`rank_host_addr`] with live context: non-loopback unicast IPv4 plus the
/// OS default-route source. Gathered per call — interfaces change between
/// discovery events.
pub fn pick_host_addr(
    candidates: &[Ipv4Addr],
    host_declared: Option<Ipv4Addr>,
) -> Option<Ipv4Addr> {
    rank_host_addr(
        candidates,
        host_declared,
        &local_ipv4s(),
        routed_local_ipv4(),
    )
}

/// A browser has no interface list; ranking falls through to the routed source address.
#[cfg(target_family = "wasm")]
fn local_ipv4s() -> Vec<Ipv4Addr> {
    Vec::new()
}

#[cfg(not(target_family = "wasm"))]
fn local_ipv4s() -> Vec<Ipv4Addr> {
    if_addrs::get_if_addrs()
        .map(|ifs| {
            ifs.into_iter()
                .filter_map(|i| match i.ip() {
                    IpAddr::V4(v) if !v.is_loopback() => Some(v),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// UDP `connect` does a route lookup without sending; `local_addr` is the
/// source the OS chose. Same as the host's `primary_local_ip`.
fn routed_local_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v) if !v.is_loopback() => Some(v),
        _ => None,
    }
}

/// The `proto` TXT a compatible host advertises. Absent is an older host.
const PROTO: &str = "punktfunk/1";

/// One resolved `_punktfunk._udp` advert.
#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveredHost {
    /// Advertised host id, or the mDNS fullname when `id` is absent.
    pub key: String,
    /// mDNS service fullname. A removal names the advert by this.
    pub fullname: String,
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// Certificate fingerprint to pin (lowercase hex). Empty if not advertised.
    pub fp_hex: String,
    /// `"required"` or `"optional"`.
    pub pair: String,
    /// Management API port from mDNS `mgmt`. `None` if absent or `0`; the library
    /// client then uses the well-known default.
    pub mgmt_port: Option<u16>,
    /// Wake-on-LAN MACs from mDNS `mac` (comma-separated `aa:bb:cc:dd:ee:ff`). Empty if absent.
    pub mac: Vec<String>,
    /// OS-identity chain from mDNS `os` (`windows` | `macos` | `linux[/<family>][/<id>]`),
    /// sanitized ([`sanitize_os`]). Empty if absent.
    pub os: String,
    /// The punktfunk protocols the host answers, from mDNS `wire` (`1,2`). Empty from a host
    /// that does not say, which answers `punktfunk/1`.
    pub wire: Vec<u8>,
}

impl DiscoveredHost {
    /// Advertised mDNS TXT `id`, or `""` when absent. [`DiscoveredHost::key`] then
    /// equals `fullname` — that equality is the no-id signal; use this, do not re-derive.
    pub fn advertised_id(&self) -> &str {
        if self.key == self.fullname {
            ""
        } else {
            &self.key
        }
    }
}

/// The host a resolved advert describes, or `None` when it is no usable punktfunk host:
/// another `proto` (some other service sharing the type) or no IPv4 address. IPv4 only:
/// clients dial `{host}:{port}`, which a bare IPv6 literal cannot parse. `txt` reads one
/// TXT value; every value is unauthenticated.
pub fn advert_from_txt<'a>(
    fullname: &str,
    port: u16,
    v4: &[Ipv4Addr],
    txt: impl Fn(&str) -> Option<&'a str>,
) -> Option<DiscoveredHost> {
    let val = |k: &str| txt(k).unwrap_or("");
    let proto = val("proto");
    if !proto.is_empty() && proto != PROTO {
        return None;
    }
    let addr = pick_host_addr(v4, val("addr").parse().ok())?.to_string();
    let id = val("id");
    Some(DiscoveredHost {
        key: if id.is_empty() { fullname } else { id }.to_string(),
        fullname: fullname.to_string(),
        name: fullname.split('.').next().unwrap_or("?").to_string(),
        addr,
        port,
        fp_hex: val("fp").to_string(),
        pair: val("pair").to_string(),
        // Absent, unparsable and `0` all mean "not advertised".
        mgmt_port: val("mgmt").parse().ok().filter(|&p| p != 0),
        mac: val("mac")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        os: sanitize_os(val("os")),
        wire: val("wire")
            .split(',')
            .filter_map(|w| w.trim().parse().ok())
            .take(8)
            .collect(),
    })
}

/// Untrusted mDNS `os=` TXT, cut to at most five lowercase `[a-z0-9._-]` tokens of 32.
/// Empty is an older host that does not advertise `os`.
pub fn sanitize_os(raw: &str) -> String {
    let tokens: Vec<String> = raw
        .to_lowercase()
        .split('/')
        .map(|t| {
            t.chars()
                .filter(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
                })
                .take(32)
                .collect::<String>()
        })
        .filter(|t| !t.is_empty())
        .take(5)
        .collect();
    tokens.join("/")
}

/// Bracket a bare IPv6 literal so `SocketAddr` parse succeeds (`fd00::1` → `[fd00::1]:4770`).
/// Without brackets the joined string never parses and the error blames the caller's input.
/// V4, hostnames, and already-bracketed input pass through. A v6 dial still fails at connect
/// while the sockets are IPv4-bound.
pub fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::{advert_from_txt, rank_host_addr, sanitize_os};
    use std::net::Ipv4Addr;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    // Shared LAN + overlay: LAN must win, with or without the host's TXT declaration.
    #[test]
    fn shared_lan_beats_shared_overlay() {
        let candidates = [ip("192.168.196.206"), ip("192.168.1.170")];
        let locals = [ip("192.168.1.150"), ip("192.168.196.57")];
        for declared in [None, Some(ip("192.168.1.170"))] {
            assert_eq!(
                rank_host_addr(&candidates, declared, &locals, Some(ip("192.168.1.150"))),
                Some(ip("192.168.1.170"))
            );
        }
    }

    // Overlay-only client: declared LAN is off-link, so it must not beat the overlay address.
    #[test]
    fn overlay_only_client_ignores_the_declared_lan_address() {
        let candidates = [ip("192.168.1.170"), ip("192.168.196.206")];
        let locals = [ip("10.1.2.3"), ip("192.168.196.57")];
        assert_eq!(
            rank_host_addr(
                &candidates,
                Some(ip("192.168.1.170")),
                &locals,
                Some(ip("10.1.2.3"))
            ),
            Some(ip("192.168.196.206"))
        );
    }

    // Same-LAN multi-NIC tie: host declaration wins; without it, lowest address.
    #[test]
    fn declared_addr_settles_a_multi_nic_tie() {
        let candidates = [ip("192.168.1.170"), ip("192.168.1.171")];
        let locals = [ip("192.168.1.150")];
        assert_eq!(
            rank_host_addr(
                &candidates,
                Some(ip("192.168.1.171")),
                &locals,
                Some(ip("192.168.1.150"))
            ),
            Some(ip("192.168.1.171"))
        );
        assert_eq!(
            rank_host_addr(&candidates, None, &locals, Some(ip("192.168.1.150"))),
            Some(ip("192.168.1.170")),
            "no declaration: lowest address, never a hash-order roll"
        );
    }

    #[test]
    fn no_context_is_still_deterministic() {
        let candidates = [ip("10.0.0.9"), ip("10.0.0.5")];
        assert_eq!(
            rank_host_addr(&candidates, None, &[], None),
            Some(ip("10.0.0.5"))
        );
        assert_eq!(rank_host_addr(&[], None, &[], None), None);
    }

    fn advert(txt: &[(&str, &str)]) -> Option<super::DiscoveredHost> {
        advert_from_txt(
            "desk._punktfunk._udp.local.",
            9777,
            &[ip("192.168.1.9")],
            |k| txt.iter().find(|(key, _)| *key == k).map(|(_, v)| *v),
        )
    }

    /// `wire` lists the protocols a host answers; an older host says nothing, junk is skipped.
    #[test]
    fn an_advert_names_the_protocols_its_host_answers() {
        assert_eq!(advert(&[("wire", "1,2")]).unwrap().wire, vec![1, 2]);
        assert_eq!(advert(&[("wire", "1, x,2")]).unwrap().wire, vec![1, 2]);
        assert!(advert(&[]).unwrap().wire.is_empty());
    }

    #[test]
    fn an_advert_reads_into_the_host_record() {
        let h = advert(&[
            ("proto", "punktfunk/1"),
            ("id", "id-1"),
            ("fp", "ab12"),
            ("pair", "required"),
            ("mgmt", "47991"),
            ("mac", " aa:bb:cc:dd:ee:01 ,,aa:bb:cc:dd:ee:02"),
            ("os", "Linux/Fedora"),
        ])
        .unwrap();
        assert_eq!(h.key, "id-1");
        assert_eq!(h.advertised_id(), "id-1");
        assert_eq!(h.name, "desk");
        assert_eq!((h.addr.as_str(), h.port), ("192.168.1.9", 9777));
        assert_eq!((h.fp_hex.as_str(), h.pair.as_str()), ("ab12", "required"));
        assert_eq!(h.mgmt_port, Some(47991));
        assert_eq!(h.mac, ["aa:bb:cc:dd:ee:01", "aa:bb:cc:dd:ee:02"]);
        assert_eq!(h.os, "linux/fedora");
    }

    #[test]
    fn an_older_host_reads_with_defaults() {
        let h = advert(&[]).unwrap();
        assert_eq!(h.key, "desk._punktfunk._udp.local.");
        assert_eq!(h.advertised_id(), "");
        assert_eq!(h.mgmt_port, None);
        assert!(h.mac.is_empty() && h.os.is_empty());
    }

    #[test]
    fn another_proto_or_no_ipv4_is_no_host() {
        assert!(advert(&[("proto", "sunshine/1")]).is_none());
        assert!(advert_from_txt("x.local.", 9777, &[], |_| None).is_none());
    }

    #[test]
    fn mgmt_zero_is_not_an_advertised_port() {
        assert_eq!(advert(&[("mgmt", "0")]).unwrap().mgmt_port, None);
        assert_eq!(advert(&[("mgmt", "70000")]).unwrap().mgmt_port, None);
    }

    #[test]
    fn sanitize_passes_well_formed_chains() {
        assert_eq!(sanitize_os("windows"), "windows");
        assert_eq!(sanitize_os("linux/fedora/bazzite"), "linux/fedora/bazzite");
        assert_eq!(
            sanitize_os("linux/opensuse/opensuse-tumbleweed"),
            "linux/opensuse/opensuse-tumbleweed"
        );
    }

    #[test]
    fn sanitize_folds_case_and_drops_junk() {
        assert_eq!(sanitize_os("Linux/Fedora"), "linux/fedora");
        assert_eq!(sanitize_os("linux/fe do ra!/§"), "linux/fedora");
        assert_eq!(sanitize_os("///"), "");
        assert_eq!(sanitize_os(""), "");
    }

    #[test]
    fn sanitize_caps_token_length_and_count() {
        let long = "x".repeat(80);
        assert_eq!(sanitize_os(&long), "x".repeat(32));
        assert_eq!(sanitize_os("a/b/c/d/e/f/g"), "a/b/c/d/e");
    }
}
