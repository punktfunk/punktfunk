//! Linux/Android batched UDP: `sendmmsg`/`recvmmsg` plus Linux UDP GSO.
//! Platform bodies of [`super::UdpTransport`]'s `send_batch`/`send_gso`/`recv_batch`.

// Crate-wide deny(unsafe_code) carve-out (lib.rs): platform syscall-batching glue —
// `sendmmsg`/`recvmmsg`/GSO move caller-owned buffers; nothing here interprets network bytes.
#![allow(unsafe_code)]

use super::{is_transient_io, UdpTransport};

#[cfg(target_os = "android")]
mod android_mmsg {
    #[repr(C)]
    #[allow(non_camel_case_types)]
    pub struct mmsghdr {
        pub msg_hdr: libc::msghdr,
        pub msg_len: libc::c_uint,
    }
    unsafe extern "C" {
        pub fn sendmmsg(
            sockfd: libc::c_int,
            msgvec: *mut mmsghdr,
            vlen: libc::c_uint,
            flags: libc::c_int,
        ) -> libc::c_int;
        pub fn recvmmsg(
            sockfd: libc::c_int,
            msgvec: *mut mmsghdr,
            vlen: libc::c_uint,
            flags: libc::c_int,
            timeout: *mut libc::timespec,
        ) -> libc::c_int;
    }
}
#[cfg(target_os = "android")]
use android_mmsg::{mmsghdr, recvmmsg, sendmmsg};
#[cfg(target_os = "linux")]
use libc::{mmsghdr, recvmmsg, sendmmsg};

/// Each returned header holds a raw pointer into `iovs`. The caller must keep
/// `iovs` alive and unmoved while those headers are passed to the syscall.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn mmsghdrs(iovs: &mut [libc::iovec]) -> Vec<mmsghdr> {
    iovs.iter_mut()
        .map(|iov| {
            // SAFETY: `mmsghdr` is a `repr(C)` POD of scalars and pointers, so all-zeroes is a
            // valid bit pattern; every field the kernel reads is assigned right below.
            let mut h: mmsghdr = unsafe { std::mem::zeroed() };
            h.msg_hdr.msg_iov = iov;
            h.msg_hdr.msg_iovlen = 1;
            h
        })
        .collect()
}

/// Process-wide UDP GSO gate. Opt-in (`PUNKTFUNK_GSO=1`), or per transport
/// through [`Transport::set_gso`](super::super::Transport::set_gso) when the
/// pacer runs at a proven link rate.
///
/// Super-buffer trains cut send CPU but lose delivered rate on constrained hops
/// (queue drop in the transport path, not the video pacer). Default stays off;
/// evidence: `design/throughput-beyond-1gbps.md`.
///
/// The gate is value-aware: `PUNKTFUNK_GSO=0` disables. Do not key on env
/// presence — `=0` would enable here while Windows USO treats `=0` as off.
#[cfg(target_os = "linux")]
mod gso {
    use std::sync::atomic::{AtomicU8, Ordering};
    // 0 = uninit, 1 = env on, 2 = env off, 3 = refused by the path.
    static STATE: AtomicU8 = AtomicU8::new(0);

    pub fn env_on() -> bool {
        match STATE.load(Ordering::Relaxed) {
            1 => true,
            2 | 3 => false,
            _ => {
                let on = std::env::var("PUNKTFUNK_GSO").is_ok_and(|v| v != "0");
                STATE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
                on
            }
        }
    }
    /// The path refused GSO once; no ask turns it back on.
    pub fn refused() -> bool {
        STATE.load(Ordering::Relaxed) == 3
    }
    /// Latch GSO off after an unsupported-path syscall error. Warn once so a
    /// mid-session downshift to `sendmmsg` is visible.
    pub fn disable() {
        if STATE.swap(3, Ordering::Relaxed) != 3 {
            tracing::warn!("Linux UDP GSO unsupported on this path — falling back to sendmmsg");
        }
    }
}

/// Errors that mean GSO is unusable on this kernel/NIC/path — latch off and
/// fall back to `sendmmsg` instead of tearing the stream down.
///
/// `EMSGSIZE`: the kernel checks each GSO segment against the device MTU;
/// `sendmmsg` does not. Treat it as "no GSO here".
#[cfg(target_os = "linux")]
fn gso_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOPROTOOPT)
            | Some(libc::EOPNOTSUPP)
            | Some(libc::EINVAL)
            | Some(libc::EIO)
            | Some(libc::EMSGSIZE)
    )
}

/// One `sendmsg` with `UDP_SEGMENT`: kernel splits `buf` into `gso_size`-byte
/// datagrams (last segment may be shorter). `EAGAIN` is `WouldBlock`; the
/// caller treats it as a lossy drop.
#[cfg(target_os = "linux")]
fn send_one_gso(fd: libc::c_int, buf: &[u8], gso_size: u16) -> std::io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: buf.as_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    // 64 B > CMSG_SPACE(2) for one `UDP_SEGMENT` u16. The union forces `cmsghdr`
    // alignment; `CMSG_FIRSTHDR` requires it.
    #[repr(C)]
    union CmsgBuf {
        _align: libc::cmsghdr,
        bytes: [u8; 64],
    }
    let mut control = CmsgBuf { bytes: [0u8; 64] };
    // SAFETY: `msghdr` is a `repr(C)` POD of scalars and pointers, so all-zeroes is a valid bit
    // pattern; every field the kernel reads is assigned below before the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    // SAFETY: `control` and `iov` are locals that outlive the call. `msg_controllen` is set to
    // `CMSG_SPACE(size_of::<u16>())`, which the 64-byte `CmsgBuf` covers, so the kernel cannot write
    // past it; `CMSG_FIRSTHDR`/`CMSG_DATA` are the documented accessors for that buffer.
    let rc = unsafe {
        msg.msg_control = control.bytes.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<u16>() as u32) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_UDP;
        (*cmsg).cmsg_type = libc::UDP_SEGMENT;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<u16>() as u32) as _;
        std::ptr::copy_nonoverlapping(
            (&gso_size as *const u16) as *const u8,
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<u16>(),
        );
        libc::sendmsg(fd, &msg, 0)
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn send_batch(t: &UdpTransport, packets: &[&[u8]]) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;
    const CHUNK: usize = 64;
    let fd = t.socket.as_raw_fd();
    let mut total_sent = 0usize;
    for chunk in packets.chunks(CHUNK) {
        // `hdrs` hold raw pointers into `iovs`; both must outlive `sendmmsg`.
        let mut iovs: Vec<libc::iovec> = chunk
            .iter()
            .map(|p| libc::iovec {
                iov_base: p.as_ptr() as *mut libc::c_void,
                iov_len: p.len(),
            })
            .collect();
        let mut hdrs = mmsghdrs(&mut iovs);
        // SAFETY: `fd` is the live socket, and `hdrs` is a local slice of `mmsghdr` whose length
        // is passed alongside it; each header points at an `iov` in `iovs`, which outlives the
        // call. The kernel only reads the buffers and writes each header's `msg_len`.
        let n = unsafe { sendmmsg(fd, hdrs.as_mut_ptr(), hdrs.len() as libc::c_uint, 0) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            // Send buffer full or stale ICMP on a connected socket: drop this chunk
            // and the rest. Only a non-transient error tears the session down.
            if is_transient_io(&err) {
                break;
            }
            return Err(err);
        }
        total_sent += n as usize;
        if (n as usize) < chunk.len() {
            break; // partial sendmmsg: drop the remainder (lossy)
        }
    }
    Ok(total_sent)
}

#[cfg(target_os = "linux")]
pub(super) fn send_gso(t: &UdpTransport, packets: &[&[u8]]) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;
    if packets.is_empty() {
        return Ok(0);
    }
    if gso::refused() || !(t.gso_wanted() || gso::env_on()) {
        return send_batch(t, packets);
    }
    let Some(seg) = super::uniform_segment(packets) else {
        return send_batch(t, packets);
    };
    let fd = t.socket.as_raw_fd();
    // 64-segment kernel cap, and 65535 - 40 - 8 (IPv6+UDP; tighter than IPv4
    // 65507). Oversize is EMSGSIZE, which `gso_unsupported` latches GSO off
    // process-wide.
    const GSO_MAX_PAYLOAD: usize = 65535 - 40 - 8;
    let max_seg = (GSO_MAX_PAYLOAD / seg).clamp(1, 64);
    let send_one = |buf: &[u8], seg| send_one_gso(fd, buf, seg);
    match super::send_segmented(packets, seg, max_seg, send_one, gso_unsupported)? {
        super::Segmented::Sent(n) => Ok(n),
        super::Segmented::Unsupported(n) => {
            gso::disable();
            Ok(n + send_batch(t, &packets[n..])?)
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn recv_batch(
    t: &UdpTransport,
    out: &mut [Vec<u8>],
    lens: &mut [usize],
) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;
    let fd = t.socket.as_raw_fd();
    let n_bufs = out.len().min(lens.len());
    if n_bufs == 0 {
        return Ok(0);
    }
    // `hdrs` hold raw pointers into `iovs`; both must outlive `recvmmsg`.
    let mut iovs: Vec<libc::iovec> = out[..n_bufs]
        .iter_mut()
        .map(|b| libc::iovec {
            iov_base: b.as_mut_ptr() as *mut libc::c_void,
            iov_len: b.len(),
        })
        .collect();
    let mut hdrs = mmsghdrs(&mut iovs);
    // SAFETY: `fd` is the live socket, and `hdrs` is a local slice of `mmsghdr` whose length is
    // passed alongside it; each header points at an `iov` backed by a buffer in `out`, which
    // outlives the call, so the kernel writes only inside those buffers.
    let n = unsafe {
        recvmmsg(
            fd,
            hdrs.as_mut_ptr(),
            n_bufs as libc::c_uint,
            libc::MSG_DONTWAIT as _,
            std::ptr::null_mut(),
        )
    };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if is_transient_io(&err) {
            return Ok(0);
        }
        return Err(err);
    }
    for (i, h) in hdrs[..n as usize].iter().enumerate() {
        lens[i] = h.msg_len as usize;
    }
    Ok(n as usize)
}

/// Block until `socket` has a datagram to read or `timeout` passes; `true` when readable. An
/// interrupted wait reads as a timeout: the caller loops anyway.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Kernel UDP GSO must emit N datagrams of `gso_size`. Loopback usually
    /// supports it; skip if this kernel does not.
    #[cfg(target_os = "linux")]
    #[test]
    fn gso_segments_into_separate_datagrams() {
        use std::os::fd::AsRawFd;
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let rx_addr = rx.local_addr().unwrap();
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.connect(rx_addr).unwrap();

        let seg = 1000usize;
        let segs = 5usize;
        let mut buf = vec![0u8; seg * segs];
        for i in 0..segs {
            buf[i * seg..(i + 1) * seg].fill(i as u8 + 1);
        }
        if let Err(e) = send_one_gso(tx.as_raw_fd(), &buf, seg as u16) {
            if gso_unsupported(&e) {
                eprintln!("UDP GSO unsupported on this kernel — skipping");
                return;
            }
            panic!("gso sendmsg failed: {e}");
        }
        let mut rbuf = vec![0u8; 4096];
        for i in 0..segs {
            let n = rx.recv(&mut rbuf).expect("recv GSO segment");
            assert_eq!(n, seg, "segment {i} should be a full {seg}-byte datagram");
            assert!(
                rbuf[..n].iter().all(|&b| b == i as u8 + 1),
                "segment {i} content"
            );
        }
    }
}
