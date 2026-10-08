//! What the OS says about the interface behind a socket: its kind and link speed. Linux
//! reads `/sys/class/net`; every other platform says "the OS did not say" until its reader
//! lands. A fact nobody could sample is `0`, never a guess.

// Crate-wide deny(unsafe_code) carve-out (lib.rs): one `ioctl` into a record this function
// owns, on Apple platforms; nothing here interprets network bytes.
#![allow(unsafe_code)]

pub use super::LinkFacts;
use std::net::IpAddr;

/// The interface holding `ip`, by name; `None` when no interface has it.
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
        ifm_name: [0; libc::IFNAMSIZ],
        ifm_current: 0,
        ifm_mask: 0,
        ifm_status: 0,
        ifm_active: 0,
        ifm_count: 0,
        ifm_ulist: std::ptr::null_mut(),
    };
    for (dst, src) in req
        .ifm_name
        .iter_mut()
        .zip(iface.bytes().take(libc::IFNAMSIZ - 1))
    {
        *dst = src as libc::c_char;
    }
    // SAFETY: `req` is the ioctl's documented in/out record, sized and NUL-terminated here;
    // `ifm_ulist` is null so the kernel fills counts only. The fd outlives the call.
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
/// IEEE 802.11 71) and `TransmitLinkSpeed` in bits per second.
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
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut len: u32 = 16 * 1024;
        let mut buf: Vec<u8> = Vec::new();
        for _ in 0..3 {
            buf.resize(len as usize, 0);
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
                _ => return LinkFacts::default(),
            }
        }
        let mut adapter = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        while !adapter.is_null() {
            // SAFETY: every pointer walked here was written by `GetAdaptersAddresses` into
            // `buf`, which outlives the loop; each is checked for null before the read.
            let facts = unsafe {
                let a = &*adapter;
                let mut unicast = a.FirstUnicastAddress;
                let mut hit = false;
                while !unicast.is_null() {
                    let sa = (*unicast).Address.lpSockaddr;
                    if !sa.is_null() {
                        let family = u32::from((*sa).sa_family);
                        let addr = if family == u32::from(AF_INET) {
                            let v4 = &*sa.cast::<SOCKADDR_IN>();
                            Some(IpAddr::from(v4.sin_addr.S_un.S_addr.to_ne_bytes()))
                        } else if family == u32::from(AF_INET6) {
                            let v6 = &*sa.cast::<SOCKADDR_IN6>();
                            Some(IpAddr::from(v6.sin6_addr.u.Byte))
                        } else {
                            None
                        };
                        hit |= addr == Some(ip);
                    }
                    unicast = (*unicast).Next;
                }
                hit.then(|| LinkFacts {
                    kind: match a.IfType {
                        IF_TYPE_ETHERNET_CSMACD => IFACE_KIND_ETHERNET,
                        IF_TYPE_IEEE80211 => IFACE_KIND_WIFI,
                        _ => IFACE_KIND_OTHER,
                    },
                    mbps: (a.TransmitLinkSpeed / 1_000_000).min(u64::from(u32::MAX)) as u32,
                })
            };
            if let Some(f) = facts {
                return f;
            }
            // SAFETY: as above; `Next` is null at the end of the list.
            adapter = unsafe { (*adapter).Next };
        }
        LinkFacts::default()
    }
}
