//! What the media path still needs from raw UDP: [`wait_readable`] for the shared socket's
//! reader, which quinn-udp keeps non-blocking, and on Linux [`send_with_tos`], a quinn-udp send
//! that carries a DSCP mark. The shared socket batches through quinn-udp itself.

/// Block until `socket` has a datagram to read or `timeout` passes; `true` when readable. An
/// interrupted wait reads as a timeout: the caller loops anyway.
#[cfg(all(unix, not(target_family = "wasm")))]
#[allow(unsafe_code)]
pub(crate) fn wait_readable(
    socket: &std::net::UdpSocket,
    timeout: std::time::Duration,
) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut pfd = libc::pollfd {
        fd: socket.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: `pfd` is one initialised pollfd that outlives the call, and its fd stays open
    // for the call because `socket` is borrowed.
    let n = unsafe { libc::poll(&mut pfd, 1, ms) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        return if e.kind() == std::io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(e)
        };
    }
    Ok(n > 0)
}

/// Block until `socket` has a datagram to read or `timeout` passes; `true` when readable.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
pub(crate) fn wait_readable(
    socket: &std::net::UdpSocket,
    timeout: std::time::Duration,
) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{WSAPoll, POLLRDNORM, SOCKET_ERROR, WSAPOLLFD};
    let mut pfd = WSAPOLLFD {
        fd: socket.as_raw_socket() as usize,
        events: POLLRDNORM,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `pfd` is one initialised WSAPOLLFD that outlives the call, and its socket stays
    // open for the call because `socket` is borrowed.
    let n = unsafe { WSAPoll(&mut pfd, 1, ms) };
    if n == SOCKET_ERROR {
        return Err(std::io::Error::last_os_error());
    }
    Ok(n > 0)
}

/// One send of `t` with `tos` as its TOS byte (IPv4, v4-mapped) or traffic class (IPv6). It
/// carries what quinn-udp's own send does: `UDP_SEGMENT` when `t` holds more than one
/// segment, the source address as `IP_PKTINFO` / `IPV6_PKTINFO`. Errors come back unhandled.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn send_with_tos(
    socket: &std::net::UdpSocket,
    t: &quinn::udp::Transmit,
    tos: u8,
) -> std::io::Result<()> {
    use std::net::IpAddr;
    let mut ctrl = Control {
        buf: [0; CONTROL],
        len: 0,
    };
    let tos = libc::c_int::from(tos).to_ne_bytes();
    match t.destination.ip() {
        IpAddr::V6(ip) if ip.to_ipv4_mapped().is_none() => {
            ctrl.push(libc::IPPROTO_IPV6, libc::IPV6_TCLASS, &tos)
        }
        _ => ctrl.push(libc::IPPROTO_IP, libc::IP_TOS, &tos),
    }
    if let Some(segment) = t.segment_size.filter(|&s| s < t.contents.len()) {
        ctrl.push(
            libc::SOL_UDP,
            libc::UDP_SEGMENT,
            &(segment as u16).to_ne_bytes(),
        );
    }
    match t.src_ip {
        // `in_pktinfo`: interface, source, destination.
        Some(IpAddr::V4(ip)) => {
            let mut info = [0; 12];
            info[4..8].copy_from_slice(&ip.octets());
            ctrl.push(libc::IPPROTO_IP, libc::IP_PKTINFO, &info);
        }
        // `in6_pktinfo`: source, interface.
        Some(IpAddr::V6(ip)) => {
            let mut info = [0; 20];
            info[..16].copy_from_slice(&ip.octets());
            ctrl.push(libc::IPPROTO_IPV6, libc::IPV6_PKTINFO, &info);
        }
        None => {}
    }
    let to = socket2::SockAddr::from(t.destination);
    let bufs = [std::io::IoSlice::new(t.contents)];
    let msg = socket2::MsgHdr::new()
        .with_addr(&to)
        .with_buffers(&bufs)
        .with_control(&ctrl.buf[..ctrl.len]);
    loop {
        match socket2::SockRef::from(socket).sendmsg(&msg, 0) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            sent => return sent.map(drop),
        }
    }
}

/// Room for the three control messages [`send_with_tos`] writes: the mark, a segment size and
/// an IPv6 source, 88 bytes on a 64-bit target.
#[cfg(any(target_os = "linux", target_os = "android"))]
const CONTROL: usize = 88;

/// Control messages laid out as the kernel reads them: a `size_t` length, the level and type,
/// then the data at the next word boundary. Aligned like a `cmsghdr`.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[repr(C, align(8))]
struct Control {
    buf: [u8; CONTROL],
    len: usize,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl Control {
    fn push(&mut self, level: libc::c_int, kind: libc::c_int, data: &[u8]) {
        const WORD: usize = std::mem::size_of::<usize>();
        let align = |n: usize| (n + WORD - 1) & !(WORD - 1);
        let head = align(std::mem::size_of::<libc::cmsghdr>());
        let at = self.len;
        let msg = &mut self.buf[at..at + head + align(data.len())];
        msg[..WORD].copy_from_slice(&(head + data.len()).to_ne_bytes());
        msg[WORD..WORD + 4].copy_from_slice(&level.to_ne_bytes());
        msg[WORD + 4..WORD + 8].copy_from_slice(&kind.to_ne_bytes());
        msg[head..head + data.len()].copy_from_slice(data);
        self.len = at + msg.len();
    }
}
