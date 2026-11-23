//! What the OS says about the interface behind a socket: its kind, link speed and MTU. Linux
//! reads `/sys/class/net`, Apple an `ioctl`, Windows the adapter list; any other platform says
//! "the OS did not say". A fact nobody could sample is `0`, never a guess.

// Crate-wide deny(unsafe_code) carve-out (lib.rs): `ioctl`s into records these functions own
// (Apple) and the adapter-list walk (Windows); nothing here interprets network bytes.
#![allow(unsafe_code)]

pub use super::LinkFacts;
use std::net::IpAddr;

/// The interface holding `ip`, by name; `None` when no interface has it.
#[cfg(target_os = "horizon")]
pub fn iface_for(_ip: IpAddr) -> Option<String> {
    None
}

/// The interface holding `ip`, by name; `None` when no interface has it.
#[cfg(not(target_os = "horizon"))]
pub fn iface_for(ip: IpAddr) -> Option<String> {
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .find(|i| i.ip() == ip)
        .map(|i| i.name)
}

/// Facts of the interface holding `ip`. Windows finds the adapter by address itself; the
/// others go by the interface's name.
pub fn link_facts(ip: IpAddr) -> LinkFacts {
    #[cfg(windows)]
    {
        windows::link_facts_by_ip(ip)
    }
    #[cfg(not(windows))]
    {
        iface_for(ip).map_or(LinkFacts::default(), |name| link_facts_of(&name))
    }
}

/// Facts of the interface this device reaches `toward` through, or its default route's when
/// `None`: the IPv4 one, else the IPv6 one of a device with no IPv4 route. The lookups target
/// documentation addresses (RFC 5737 192.0.2.1, RFC 3849 2001:db8::1), which no LAN holds.
pub fn local_link_facts(toward: Option<IpAddr>) -> LinkFacts {
    let ip = match toward {
        Some(to) => local_ip_toward(to),
        None => local_ip_toward(std::net::Ipv4Addr::new(192, 0, 2, 1).into()).or_else(|| {
            local_ip_toward(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).into())
        }),
    };
    ip.map_or(LinkFacts::default(), link_facts)
}

/// MTU of the interface this device reaches `peer` through; `0` when the OS did not say.
pub fn mtu_toward(peer: IpAddr) -> u32 {
    let Some(ip) = local_ip_toward(peer) else {
        return 0;
    };
    #[cfg(windows)]
    {
        windows::mtu_by_ip(ip)
    }
    #[cfg(not(windows))]
    {
        iface_for(ip).map_or(0, |name| mtu_of(&name))
    }
}

/// The address this device sends to `to` from. A UDP `connect` is a route lookup: nothing is
/// sent. A v4-mapped `to` is looked up as v4, so the answer matches the interface list.
fn local_ip_toward(to: IpAddr) -> Option<IpAddr> {
    let to = to.to_canonical();
    let any: IpAddr = if to.is_ipv4() {
        std::net::Ipv4Addr::UNSPECIFIED.into()
    } else {
        std::net::Ipv6Addr::UNSPECIFIED.into()
    };
    let s = std::net::UdpSocket::bind((any, 0)).ok()?;
    s.connect((to, 9)).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn mtu_of(iface: &str) -> u32 {
    std::fs::read_to_string(format!("/sys/class/net/{iface}/mtu"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// `SIOCGIFMTU`; refused reads as "the OS did not say".
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
fn mtu_of(iface: &str) -> u32 {
    use std::os::fd::AsRawFd;
    /// `struct ifreq` with the union read as `ifru_mtu`: 32 bytes.
    #[repr(C)]
    struct IfMtuReq {
        ifr_name: [libc::c_char; libc::IFNAMSIZ],
        ifr_mtu: libc::c_int,
        _rest: [u8; 12],
    }
    const SIOCGIFMTU: libc::c_ulong = 0xc020_6933;
    let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") else {
        return 0;
    };
    let mut req = IfMtuReq {
        ifr_name: apple_ifname(iface),
        ifr_mtu: 0,
        _rest: [0; 12],
    };
    // SAFETY: `req` is the ioctl's documented 32-byte in/out record with a NUL-terminated
    // name. The fd outlives the call.
    let rc = unsafe { libc::ioctl(sock.as_raw_fd(), SIOCGIFMTU, &mut req) };
    if rc != 0 {
        return 0;
    }
    u32::try_from(req.ifr_mtu).unwrap_or(0)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    windows
)))]
fn mtu_of(_iface: &str) -> u32 {
    0
}

/// `iface` as an `ifreq` name: truncated to fit, NUL-terminated.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
fn apple_ifname(iface: &str) -> [libc::c_char; libc::IFNAMSIZ] {
    let mut name = [0; libc::IFNAMSIZ];
    for (dst, src) in name.iter_mut().zip(iface.bytes().take(libc::IFNAMSIZ - 1)) {
        *dst = src as libc::c_char;
    }
    name
}

/// Wi-Fi has a `wireless/` directory, a NIC a `device/` link; anything else is
/// [`IFACE_KIND_OTHER`]. `speed` reads `-1` or nothing where the driver does not say.
/// Android is the same kernel; an app that may not read `/sys` gets "other" and `0`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn link_facts_of(iface: &str) -> LinkFacts {
    use super::{IFACE_KIND_ETHERNET, IFACE_KIND_OTHER, IFACE_KIND_WIFI};
    let dir = format!("/sys/class/net/{iface}");
    let kind = if std::path::Path::new(&format!("{dir}/wireless")).exists() {
        IFACE_KIND_WIFI
    } else if std::path::Path::new(&format!("{dir}/device")).exists() {
        IFACE_KIND_ETHERNET
    } else {
        IFACE_KIND_OTHER
    };
    let mbps = std::fs::read_to_string(format!("{dir}/speed"))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|&s| s > 0)
        .map_or(0, |s| s as u32);
    LinkFacts { kind, mbps }
}

/// `SIOCGIFMEDIA`: the media word's type nibble says Wi-Fi or Ethernet; the subtype says the
/// speed for the three wired rates every Apple header agrees on, and `0` past them. The
/// ioctl is refused on some Apple platforms, which reads as "the OS did not say".
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
pub fn link_facts_of(iface: &str) -> LinkFacts {
    use super::{IFACE_KIND_ETHERNET, IFACE_KIND_OTHER, IFACE_KIND_WIFI};
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct IfMediaReq {
        ifm_name: [libc::c_char; libc::IFNAMSIZ],
        ifm_current: libc::c_int,
        ifm_mask: libc::c_int,
        ifm_status: libc::c_int,
        ifm_active: libc::c_int,
        ifm_count: libc::c_int,
        ifm_ulist: *mut libc::c_int,
    }
    const SIOCGIFMEDIA: libc::c_ulong = 0xc028_6938;
    const IFM_NMASK: libc::c_int = 0x0000_00e0;
    const IFM_TMASK: libc::c_int = 0x0000_001f;
    const IFM_ETHER: libc::c_int = 0x20;
    const IFM_IEEE80211: libc::c_int = 0x80;
    let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") else {
        return LinkFacts::default();
    };
    let mut req = IfMediaReq {
        ifm_name: apple_ifname(iface),
        ifm_current: 0,
        ifm_mask: 0,
        ifm_status: 0,
        ifm_active: 0,
        ifm_count: 0,
        ifm_ulist: std::ptr::null_mut(),
    };
    // SAFETY: the request code is XNU's SIOCGIFMEDIA32: the kernel copies 40 bytes in and out,
    // inside `req`'s 48, and the name is NUL-terminated. Its list pointer is bytes 36..40,
    // padding here, not `ifm_ulist`; a 32-bit address lies in a 64-bit process's 4 GiB page
    // zero, so a list copy faults instead of landing. The fd outlives the call.
    let rc = unsafe { libc::ioctl(sock.as_raw_fd(), SIOCGIFMEDIA, &mut req) };
    if rc != 0 {
        return LinkFacts::default();
    }
    let kind = match req.ifm_active & IFM_NMASK {
        IFM_IEEE80211 => IFACE_KIND_WIFI,
        IFM_ETHER => IFACE_KIND_ETHERNET,
        _ => IFACE_KIND_OTHER,
    };
    let mbps = match (kind, req.ifm_active & IFM_TMASK) {
        (IFACE_KIND_ETHERNET, 3) => 10,
        (IFACE_KIND_ETHERNET, 6) => 100,
        (IFACE_KIND_ETHERNET, 16) => 1_000,
        _ => 0,
    };
    LinkFacts { kind, mbps }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos"
)))]
pub fn link_facts_of(_iface: &str) -> LinkFacts {
    LinkFacts::default()
}

/// `GetAdaptersAddresses`: the adapter holding the address says its `IfType` (Ethernet 6,
/// IEEE 802.11 71), `TransmitLinkSpeed` in bits per second, and `Mtu`.
#[cfg(windows)]
mod windows {
    use super::super::{IFACE_KIND_ETHERNET, IFACE_KIND_OTHER, IFACE_KIND_WIFI};
    use super::LinkFacts;
    use std::net::IpAddr;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
    };

    const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
    const IF_TYPE_IEEE80211: u32 = 71;
    const ERROR_BUFFER_OVERFLOW: u32 = 111;

    pub(super) fn link_facts_by_ip(ip: IpAddr) -> LinkFacts {
        adapter_by_ip(ip, |a| LinkFacts {
            kind: match a.IfType {
                IF_TYPE_ETHERNET_CSMACD => IFACE_KIND_ETHERNET,
                IF_TYPE_IEEE80211 => IFACE_KIND_WIFI,
                _ => IFACE_KIND_OTHER,
            },
            mbps: (a.TransmitLinkSpeed / 1_000_000).min(u64::from(u32::MAX)) as u32,
        })
        .unwrap_or_default()
    }

    pub(super) fn mtu_by_ip(ip: IpAddr) -> u32 {
        adapter_by_ip(ip, |a| a.Mtu).unwrap_or(0)
    }

    /// `field` of the adapter holding `ip`; `None` when none does or the list is unreadable.
    fn adapter_by_ip<T>(ip: IpAddr, field: impl Fn(&IP_ADAPTER_ADDRESSES_LH) -> T) -> Option<T> {
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut len: u32 = 16 * 1024;
        // `u64` words: the adapter structs are read in place and are 8-aligned.
        let mut buf: Vec<u64> = Vec::new();
        for _ in 0..3 {
            buf.resize((len as usize).div_ceil(8), 0);
            // SAFETY: `buf` holds `len` bytes the call may fill; the list it writes is read
            // only while `buf` lives and only through pointers the call itself laid out.
            let rc = unsafe {
                GetAdaptersAddresses(
                    AF_UNSPEC as u32,
                    flags,
                    std::ptr::null(),
                    buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                    &mut len,
                )
            };
            match rc {
                0 => break,
                ERROR_BUFFER_OVERFLOW => continue,
                _ => return None,
            }
        }
        let mut adapter = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        while !adapter.is_null() {
            // SAFETY: every pointer walked here was written by `GetAdaptersAddresses` into
            // `buf`, which outlives the loop; each is checked for null before the read.
            let found = unsafe {
                let a = &*adapter;
                let mut unicast = a.FirstUnicastAddress;
                let mut hit = false;
                while !unicast.is_null() {
                    let sa = (*unicast).Address.lpSockaddr;
                    if !sa.is_null() {
                        let family = u32::from((*sa).sa_family);
                        let addr = if family == u32::from(AF_INET) {
                            let v4 = sa.cast::<SOCKADDR_IN>().read_unaligned();
                            Some(IpAddr::from(v4.sin_addr.S_un.S_addr.to_ne_bytes()))
                        } else if family == u32::from(AF_INET6) {
                            let v6 = sa.cast::<SOCKADDR_IN6>().read_unaligned();
                            Some(IpAddr::from(v6.sin6_addr.u.Byte))
                        } else {
                            None
                        };
                        hit |= addr == Some(ip);
                    }
                    unicast = (*unicast).Next;
                }
                hit.then(|| field(a))
            };
            if found.is_some() {
                return found;
            }
            // SAFETY: as above; `Next` is null at the end of the list.
            adapter = unsafe { (*adapter).Next };
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loopback is above 1500 everywhere (Linux 65536, macOS 16384, Windows `u32::MAX`), so a
    /// reading of it proves the route lookup and this platform's reader.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn the_loopback_interface_reports_its_mtu() {
        assert!(mtu_toward(std::net::Ipv4Addr::LOCALHOST.into()) > 1500);
    }
    /// The loopback adapter is always there: finding it walks the adapter list.
    #[cfg(windows)]
    #[test]
    fn the_loopback_address_names_an_adapter() {
        let facts = link_facts(std::net::Ipv4Addr::LOCALHOST.into());
        assert_eq!(facts.kind, crate::transport::IFACE_KIND_OTHER);
    }
}
