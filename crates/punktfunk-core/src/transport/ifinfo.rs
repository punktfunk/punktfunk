//! What the OS says about the interface behind a socket: its kind and link speed. Linux
//! reads `/sys/class/net`; every other platform says "the OS did not say" until its reader
//! lands. A fact nobody could sample is `0`, never a guess.

#[cfg(target_os = "linux")]
use super::{IFACE_KIND_ETHERNET, IFACE_KIND_OTHER, IFACE_KIND_WIFI};
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

#[cfg(not(target_os = "linux"))]
pub fn link_facts_of(_iface: &str) -> LinkFacts {
    LinkFacts::default()
}
