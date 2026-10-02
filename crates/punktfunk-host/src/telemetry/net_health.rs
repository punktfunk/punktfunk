//! The network layer under the sender: the kernel's send queue for the data socket, the egress
//! NIC's drop counters, and address/route/link churn on the box.
//!
//! Every app-level line can read clean while this layer stalls: the sender hands each datagram
//! to the kernel on time and the client still starves. The sender's 30 s `wire egress` line
//! and the `network changed` events are the only witnesses for that case.

use std::net::{IpAddr, UdpSocket};

/// One sender window of wire counters. Off Linux every field stays 0 so the line still prints
/// and a reader knows the layer was not sampled rather than silent.
#[derive(Default, Debug, Clone, Copy)]
pub struct WireWindow {
    /// Peak bytes the kernel still held for the data socket, sampled once per frame.
    pub outq_max_kb: u32,
    pub tx_dropped: u64,
    pub tx_errors: u64,
    pub carrier_changes: u64,
    /// Host-wide UDP `SndbufErrors` delta; the one counter the NIC statistics miss.
    pub udp_sndbuf_errors: u64,
}

/// Samples the data socket's kernel queue and the egress interface's counters.
pub struct WireProbe {
    /// The interface holding the socket's local address; `None` when nothing matched.
    pub iface: Option<String>,
    #[cfg(target_os = "linux")]
    sock: Option<UdpSocket>,
    #[cfg(target_os = "linux")]
    last: [u64; 4],
    #[cfg(target_os = "linux")]
    outq_max: u32,
}

impl WireProbe {
    /// `sock` is a clone of the data socket; the queue is per socket, not per fd.
    pub fn new(sock: Option<UdpSocket>) -> Self {
        let iface = sock
            .as_ref()
            .and_then(|s| s.local_addr().ok())
            .and_then(|a| iface_for(a.ip()));
        #[cfg(target_os = "linux")]
        {
            let mut p = Self {
                iface,
                sock,
                last: [0; 4],
                outq_max: 0,
            };
            p.last = p.counters();
            p
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = sock;
            Self { iface }
        }
    }

    /// Once per frame: bytes the kernel has not yet put on the wire for this socket.
    pub fn sample(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(s) = &self.sock {
            use std::os::fd::AsRawFd;
            let mut n: libc::c_int = 0;
            // SAFETY: `TIOCOUTQ` on a UDP socket writes one int; the fd outlives the call.
            let ok = unsafe { libc::ioctl(s.as_raw_fd(), libc::TIOCOUTQ, &mut n) } == 0;
            if ok && n > 0 {
                self.outq_max = self.outq_max.max(n as u32);
            }
        }
    }

    /// Close the window: deltas since the previous call, peak queue reset.
    pub fn window(&mut self) -> WireWindow {
        #[cfg(target_os = "linux")]
        {
            let now = self.counters();
            let d = |i: usize| now[i].saturating_sub(self.last[i]);
            let w = WireWindow {
                outq_max_kb: self.outq_max / 1024,
                tx_dropped: d(0),
                tx_errors: d(1),
                carrier_changes: d(2),
                udp_sndbuf_errors: d(3),
            };
            self.last = now;
            self.outq_max = 0;
            w
        }
        #[cfg(not(target_os = "linux"))]
        WireWindow::default()
    }

    #[cfg(target_os = "linux")]
    fn counters(&self) -> [u64; 4] {
        let Some(iface) = &self.iface else {
            return [0, 0, 0, udp_sndbuf_errors()];
        };
        let sys = |leaf: &str| sys_u64(&format!("/sys/class/net/{iface}/{leaf}"));
        [
            sys("statistics/tx_dropped"),
            sys("statistics/tx_errors"),
            sys("carrier_changes"),
            udp_sndbuf_errors(),
        ]
    }
}

/// What this host knows about its own end of the path, for a client that asked
/// ([`punktfunk_core::quic::HostFacts`]): the data socket's interface kind and link speed
/// ([`punktfunk_core::transport::ifinfo`]), its granted send buffer, and the pinned profile.
pub fn host_facts(sock: Option<&UdpSocket>) -> punktfunk_core::quic::HostFacts {
    use punktfunk_core::quic::FORCED_PROFILE_NONE;
    let link = sock
        .and_then(|s| s.local_addr().ok())
        .map_or(Default::default(), |a| {
            punktfunk_core::transport::ifinfo::link_facts(a.ip())
        });
    let sndbuf_kb = sock
        .and_then(|s| socket2::SockRef::from(s).send_buffer_size().ok())
        .map_or(0, |b| (b / 1024) as u32);
    punktfunk_core::quic::HostFacts {
        iface_kind: link.kind,
        link_mbps: link.mbps,
        sndbuf_kb,
        forced_profile: crate::send_pacing::forced_delivery()
            .map_or(FORCED_PROFILE_NONE, |p| p as u8),
    }
}

fn iface_for(ip: IpAddr) -> Option<String> {
    punktfunk_core::transport::ifinfo::iface_for(ip)
}

#[cfg(target_os = "linux")]
fn sys_u64(path: &str) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// `Udp: … SndbufErrors …` from `/proc/net/snmp`: a names row, then a values row.
#[cfg(target_os = "linux")]
fn udp_sndbuf_errors() -> u64 {
    let Ok(s) = std::fs::read_to_string("/proc/net/snmp") else {
        return 0;
    };
    let mut rows = s.lines().filter(|l| l.starts_with("Udp:"));
    let (Some(names), Some(vals)) = (rows.next(), rows.next()) else {
        return 0;
    };
    names
        .split_whitespace()
        .position(|n| n == "SndbufErrors")
        .and_then(|i| vals.split_whitespace().nth(i))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Every IPv4 the box holds, per interface, on one startup line. Two addresses in one subnet
/// on one interface get a warning: two DHCP clients on that interface is the usual cause, and
/// each renewal then re-commits addresses and routes under a live stream.
pub fn log_addresses() {
    let mut ifs = if_addrs::get_if_addrs().unwrap_or_default();
    ifs.retain(|i| i.ip().is_ipv4() && !i.is_loopback());
    ifs.sort_by(|a, b| a.name.cmp(&b.name));
    let mut rendered = Vec::new();
    for i in &ifs {
        let if_addrs::IfAddr::V4(v4) = &i.addr else {
            continue;
        };
        rendered.push(format!("{}:{}/{}", i.name, v4.ip, v4.prefixlen));
        let twin = ifs.iter().find(|o| {
            let if_addrs::IfAddr::V4(w) = &o.addr else {
                return false;
            };
            o.name == i.name
                && w.ip != v4.ip
                && w.netmask == v4.netmask
                && (u32::from(w.ip) & u32::from(w.netmask))
                    == (u32::from(v4.ip) & u32::from(v4.netmask))
        });
        if let Some(twin) = twin {
            tracing::warn!(
                iface = %i.name,
                a = %v4.ip,
                b = %twin.ip(),
                "one interface holds two addresses in the same subnet — two DHCP clients on it \
                 (NetworkManager plus dhcpcd/networkd) is the usual cause; each renewal re-commits \
                 addresses and routes under a live stream, and clients pin whichever address \
                 answered first"
            );
        }
    }
    tracing::info!(addrs = %rendered.join(" "), "host addresses");
}

/// Log every IPv4 address and main-table route event, and every link whose flags moved,
/// for the life of the host.
#[cfg(target_os = "linux")]
pub fn spawn_route_watch() {
    let _ = std::thread::Builder::new()
        .name("punktfunk-netwatch".into())
        .spawn(route_watch);
}

#[cfg(not(target_os = "linux"))]
pub fn spawn_route_watch() {}

/// Folds a link re-announcing flags it already had. NetworkManager's periodic scan makes
/// the kernel re-emit `RTM_NEWLINK` for an idle Wi-Fi NIC every 10-25 s, unchanged; only a
/// move is churn. The rendered line carries the flags next to `iface=`, so the line itself
/// is the state — [`netlink_events`] never builds them into anything longer-lived.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct LinkDedupe(std::collections::HashMap<String, String>);

#[cfg(target_os = "linux")]
impl LinkDedupe {
    /// True when `line` is worth a log line. A `link gone` forgets the interface, so the
    /// re-add speaks again; address and route lines always pass, because an address re-add
    /// is a live re-commit and not a re-announcement.
    fn is_news(&mut self, line: &str) -> bool {
        let Some(rest) = line.strip_prefix("link ") else {
            return true;
        };
        let gone = rest.starts_with("gone ");
        let rest = rest.strip_prefix("gone ").unwrap_or(rest);
        let Some((iface, flags)) = rest.strip_prefix("iface=").and_then(|r| r.split_once(' '))
        else {
            return true;
        };
        if gone {
            self.0.remove(iface);
            return true;
        }
        self.0
            .insert(iface.to_string(), flags.to_string())
            .as_deref()
            != Some(flags)
    }
}

#[cfg(target_os = "linux")]
fn route_watch() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::{Duration, Instant};
    // SAFETY: plain socket creation; no memory of ours is involved.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    if raw < 0 {
        tracing::debug!("netlink route socket refused — no network-change log");
        return;
    }
    // SAFETY: `raw` is the fresh socket just created; `OwnedFd` is its only owner and closes it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: all-zero is a valid sockaddr_nl (family and groups are set just below).
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as u16;
    sa.nl_groups = (libc::RTMGRP_LINK | libc::RTMGRP_IPV4_IFADDR | libc::RTMGRP_IPV4_ROUTE) as u32;
    // SAFETY: `sa` is a live sockaddr_nl and the length passed is its size.
    let bound = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &sa as *const libc::sockaddr_nl as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };
    if bound < 0 {
        tracing::debug!("netlink route bind refused — no network-change log");
        return;
    }
    let mut buf = vec![0u8; 32 * 1024];
    // Rate limit: a container host adds veths in bursts. Past the budget, as for a link
    // repeating its flags, the minute closes with one count instead of a flood.
    const BUDGET: u32 = 20;
    let mut minute = Instant::now();
    let (mut spent, mut suppressed, mut repeats) = (0u32, 0u64, 0u64);
    let mut links = LinkDedupe::default();
    loop {
        // SAFETY: `buf` is writable for `buf.len()` bytes and outlives the call.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                // A burst overran the socket buffer: those events are gone, the socket is not.
                Some(libc::ENOBUFS) => {
                    suppressed += 1;
                    continue;
                }
                _ => {
                    tracing::debug!(error = %e, "netlink route watch ended — no network-change log");
                    break;
                }
            }
        }
        if minute.elapsed() >= Duration::from_secs(60) {
            if suppressed > 0 || repeats > 0 {
                tracing::info!(
                    suppressed,
                    repeats,
                    "network changed: more events not listed"
                );
            }
            minute = Instant::now();
            spent = 0;
            suppressed = 0;
            repeats = 0;
        }
        for line in netlink_events(&buf[..n as usize]) {
            if !links.is_news(&line) {
                repeats += 1;
            } else if spent < BUDGET {
                spent += 1;
                tracing::info!("network changed: {line}");
            } else {
                suppressed += 1;
            }
        }
    }
}

/// One human line per message in a netlink batch; unknown types are skipped.
#[cfg(target_os = "linux")]
fn netlink_events(mut b: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    while b.len() >= 16 {
        let len = u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as usize;
        let ty = u16::from_ne_bytes([b[4], b[5]]);
        if len < 16 || len > b.len() {
            break;
        }
        let body = &b[16..len];
        match ty {
            libc::RTM_NEWLINK | libc::RTM_DELLINK if body.len() >= 16 => {
                let index = i32::from_ne_bytes([body[4], body[5], body[6], body[7]]);
                let flags = u32::from_ne_bytes([body[8], body[9], body[10], body[11]]);
                let up = flags & libc::IFF_UP as u32 != 0;
                let running = flags & libc::IFF_RUNNING as u32 != 0;
                let lower = flags & libc::IFF_LOWER_UP as u32 != 0;
                let what = if ty == libc::RTM_NEWLINK {
                    "link"
                } else {
                    "link gone"
                };
                out.push(format!(
                    "{what} iface={} up={up} running={running} carrier={lower}",
                    ifname(index)
                ));
            }
            libc::RTM_NEWADDR | libc::RTM_DELADDR if body.len() >= 8 => {
                let prefix = body[1];
                let index = u32::from_ne_bytes([body[4], body[5], body[6], body[7]]) as i32;
                let addr = attrs(&body[8..])
                    .find(|(t, v)| {
                        (*t == libc::IFA_LOCAL || *t == libc::IFA_ADDRESS) && v.len() == 4
                    })
                    .map(|(_, v)| std::net::Ipv4Addr::new(v[0], v[1], v[2], v[3]).to_string())
                    .unwrap_or_else(|| "?".into());
                let what = if ty == libc::RTM_NEWADDR {
                    "address"
                } else {
                    "address gone"
                };
                out.push(format!(
                    "{what} iface={} addr={addr}/{prefix}",
                    ifname(index)
                ));
            }
            // Main table only: the local table echoes every address add.
            libc::RTM_NEWROUTE | libc::RTM_DELROUTE
                if body.len() >= 12 && body[4] == libc::RT_TABLE_MAIN =>
            {
                let dst_len = body[1];
                let (mut dst, mut gw, mut oif) = ("default".to_string(), None, None);
                for (t, v) in attrs(&body[12..]) {
                    match t {
                        libc::RTA_DST if v.len() == 4 => {
                            dst = format!(
                                "{}/{dst_len}",
                                std::net::Ipv4Addr::new(v[0], v[1], v[2], v[3])
                            );
                        }
                        libc::RTA_GATEWAY if v.len() == 4 => {
                            gw = Some(std::net::Ipv4Addr::new(v[0], v[1], v[2], v[3]));
                        }
                        libc::RTA_OIF if v.len() == 4 => {
                            oif = Some(i32::from_ne_bytes([v[0], v[1], v[2], v[3]]));
                        }
                        _ => {}
                    }
                }
                let what = if ty == libc::RTM_NEWROUTE {
                    "route"
                } else {
                    "route gone"
                };
                let via = gw.map(|g| format!(" via={g}")).unwrap_or_default();
                let dev = oif
                    .map(|i| format!(" iface={}", ifname(i)))
                    .unwrap_or_default();
                out.push(format!("{what} dst={dst}{via}{dev}"));
            }
            _ => {}
        }
        b = &b[nl_align(len).min(b.len())..];
    }
    out
}

#[cfg(target_os = "linux")]
fn nl_align(n: usize) -> usize {
    (n + 3) & !3
}

/// `(type, payload)` for each rtattr in `b`.
#[cfg(target_os = "linux")]
fn attrs(mut b: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        if b.len() < 4 {
            return None;
        }
        let len = u16::from_ne_bytes([b[0], b[1]]) as usize;
        let ty = u16::from_ne_bytes([b[2], b[3]]);
        if len < 4 || len > b.len() {
            return None;
        }
        let v = &b[4..len];
        b = &b[nl_align(len).min(b.len())..];
        Some((ty, v))
    })
}

#[cfg(target_os = "linux")]
fn ifname(index: i32) -> String {
    let mut name = [0 as libc::c_char; libc::IF_NAMESIZE];
    // SAFETY: `name` is IF_NAMESIZE bytes, the length if_indextoname writes at most.
    let p = unsafe { libc::if_indextoname(index as libc::c_uint, name.as_mut_ptr()) };
    if p.is_null() {
        return format!("#{index}");
    }
    // SAFETY: a non-null return is NUL-terminated within `name`.
    unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn msg(ty: u16, body: &[u8]) -> Vec<u8> {
        let len = (16 + body.len()) as u32;
        let mut m = len.to_ne_bytes().to_vec();
        m.extend_from_slice(&ty.to_ne_bytes());
        m.extend_from_slice(&[0; 10]);
        m.extend_from_slice(body);
        m.resize(nl_align(m.len()), 0);
        m
    }

    fn attr(ty: u16, v: &[u8]) -> Vec<u8> {
        let mut a = ((4 + v.len()) as u16).to_ne_bytes().to_vec();
        a.extend_from_slice(&ty.to_ne_bytes());
        a.extend_from_slice(v);
        a.resize(nl_align(a.len()), 0);
        a
    }

    #[test]
    fn an_address_add_names_the_address_and_prefix() {
        let mut body = vec![libc::AF_INET as u8, 24, 0, 0];
        body.extend_from_slice(&1i32.to_ne_bytes()); // lo: always index 1
        body.extend(attr(libc::IFA_LOCAL, &[192, 168, 178, 73]));
        let lines = netlink_events(&msg(libc::RTM_NEWADDR, &body));
        assert_eq!(
            lines,
            vec!["address iface=lo addr=192.168.178.73/24".to_string()]
        );
    }

    #[test]
    fn a_local_table_route_is_skipped_and_a_main_route_is_kept() {
        let mut local = vec![libc::AF_INET as u8, 32, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0];
        local.extend(attr(libc::RTA_DST, &[192, 168, 178, 41]));
        let mut main = vec![
            libc::AF_INET as u8,
            0,
            0,
            0,
            libc::RT_TABLE_MAIN,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        main.extend(attr(libc::RTA_GATEWAY, &[192, 168, 178, 1]));
        let mut batch = msg(libc::RTM_NEWROUTE, &local);
        batch.extend(msg(libc::RTM_DELROUTE, &main));
        let lines = netlink_events(&batch);
        assert_eq!(
            lines,
            vec!["route gone dst=default via=192.168.178.1".to_string()]
        );
    }

    const UP: u32 = libc::IFF_UP as u32;
    const UP_RUNNING: u32 = UP | libc::IFF_RUNNING as u32 | libc::IFF_LOWER_UP as u32;

    /// The line the watch would log, built by the parser itself: the dedupe keys off
    /// that exact rendering, so a format change has to fail here.
    fn link_line(ty: u16, index: i32, flags: u32) -> String {
        let mut body = vec![0u8; 16];
        body[4..8].copy_from_slice(&index.to_ne_bytes());
        body[8..12].copy_from_slice(&flags.to_ne_bytes());
        netlink_events(&msg(ty, &body))
            .pop()
            .expect("one link line")
    }

    #[test]
    fn a_link_repeating_its_flags_is_logged_once() {
        let mut d = LinkDedupe::default();
        let line = link_line(libc::RTM_NEWLINK, 1, UP);
        assert_eq!(line, "link iface=lo up=true running=false carrier=false");
        assert!(d.is_news(&line));
        assert!(!d.is_news(&line));
    }

    #[test]
    fn a_flag_move_and_the_move_back_both_log() {
        let mut d = LinkDedupe::default();
        assert!(d.is_news(&link_line(libc::RTM_NEWLINK, 1, UP)));
        assert!(d.is_news(&link_line(libc::RTM_NEWLINK, 1, UP_RUNNING)));
        // A carrier that returns is the news a carrier drop was.
        assert!(d.is_news(&link_line(libc::RTM_NEWLINK, 1, UP)));
    }

    #[test]
    fn two_interfaces_keep_their_own_flags() {
        let mut d = LinkDedupe::default();
        assert!(d.is_news(&link_line(libc::RTM_NEWLINK, 1, UP)));
        assert!(d.is_news(&link_line(libc::RTM_NEWLINK, 9999, UP)));
        assert!(!d.is_news(&link_line(libc::RTM_NEWLINK, 1, UP)));
        assert!(!d.is_news(&link_line(libc::RTM_NEWLINK, 9999, UP)));
    }

    #[test]
    fn a_link_that_went_away_logs_on_its_re_add() {
        let mut d = LinkDedupe::default();
        assert!(d.is_news(&link_line(libc::RTM_NEWLINK, 1, UP)));
        let gone = link_line(libc::RTM_DELLINK, 1, UP);
        assert_eq!(
            gone,
            "link gone iface=lo up=true running=false carrier=false"
        );
        assert!(d.is_news(&gone));
        assert!(d.is_news(&link_line(libc::RTM_NEWLINK, 1, UP)));
    }

    #[test]
    fn address_and_route_lines_never_fold() {
        let mut d = LinkDedupe::default();
        let addr = "address iface=lo addr=192.168.178.73/24";
        assert!(d.is_news(addr));
        assert!(d.is_news(addr));
        let route = "route dst=default via=192.168.178.1 iface=lo";
        assert!(d.is_news(route));
        assert!(d.is_news(route));
    }

    #[test]
    fn a_truncated_batch_stops_cleanly() {
        let mut m = msg(libc::RTM_NEWLINK, &[0; 16]);
        m.truncate(20);
        assert!(netlink_events(&m).is_empty());
    }
}
