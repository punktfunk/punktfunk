//! Per-socket receive drops: packets the kernel threw away because this socket's buffer was
//! full. Loss counted here happened on this machine, not on the network. Linux and Android
//! answer through `SO_MEMINFO`; the other platforms have no per-socket figure and answer
//! `None`, which a reader reports as "not sampled", never as zero.

use std::net::UdpSocket;

/// Packets dropped at this socket's receive buffer since it was opened.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn socket_drops(sock: &UdpSocket) -> Option<u64> {
    use std::os::fd::AsRawFd;
    // `SO_MEMINFO` (55) fills `SK_MEMINFO_VARS` (9) u32s; `SK_MEMINFO_DROPS` is index 8.
    // `libc` has neither constant on every target it builds for, so they are literal here.
    const SO_MEMINFO: libc::c_int = 55;
    const SK_MEMINFO_VARS: usize = 9;
    const SK_MEMINFO_DROPS: usize = 8;
    let mut vals = [0u32; SK_MEMINFO_VARS];
    let mut len = std::mem::size_of_val(&vals) as libc::socklen_t;
    // SAFETY: the fd is this process's live socket; the kernel writes at most `len` bytes
    // into `vals`, which outlives the call.
    let rc = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            SO_MEMINFO,
            vals.as_mut_ptr().cast(),
            &mut len,
        )
    };
    (rc == 0 && len as usize >= std::mem::size_of_val(&vals))
        .then(|| u64::from(vals[SK_MEMINFO_DROPS]))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn socket_drops(_sock: &UdpSocket) -> Option<u64> {
    None
}

/// The receive buffer the OS granted, KiB; `0` when it would not say.
pub fn recv_buffer_kb(sock: &UdpSocket) -> u32 {
    socket2::SockRef::from(sock)
        .recv_buffer_size()
        .map_or(0, |b| (b / 1024) as u32)
}
