//! What the OS says about the interface behind a socket: its kind and link speed. Linux
//! reads `/sys/class/net`; every other platform says "the OS did not say" until its reader
//! lands. A fact nobody could sample is `0`, never a guess.

// Crate-wide deny(unsafe_code) carve-out (lib.rs): one `ioctl` into a record this function
// owns, on Apple platforms; nothing here interprets network bytes.
#![allow(unsafe_code)]

use std::net::IpAddr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkFacts {
    pub kind: u8,
    /// `0` = not sampled.
    pub mbps: u32,
}

/// The interface holding `ip`, by name; `None` when no interface has it.
pub fn iface_for(ip: IpAddr) -> Option<String> {
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .find(|i| i.ip() == ip)
        .map(|i| i.name)
}

/// Facts of the interface holding `ip`.
pub fn link_facts(ip: IpAddr) -> LinkFacts {
    iface_for(ip).map_or(LinkFacts::default(), |name| link_facts_of(&name))
}

/// Wi-Fi has a `wireless/` directory, a NIC a `device/` link; anything else is
/// [`IFACE_KIND_OTHER`]. `speed` reads `-1` or nothing where the driver does not say.
#[cfg(target_os = "linux")]
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
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos"
)))]
pub fn link_facts_of(_iface: &str) -> LinkFacts {
    LinkFacts::default()
}
