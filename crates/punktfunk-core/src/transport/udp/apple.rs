//! Apple/BSD batched UDP receive: Darwin `recvmsg_x`, `recv`-loop fallback on
//! other BSDs. Platform body of [`super::UdpTransport`]'s `recv_batch`.

// Crate-wide deny(unsafe_code) carve-out (lib.rs): platform syscall-batching glue —
// `recvmsg_x` fills caller-owned buffers; nothing here interprets network bytes. Proofs at each site.
#![allow(unsafe_code)]

use super::{is_transient_io, UdpTransport};

/// Darwin batched-receive gate. No `recvmmsg(2)` here, so without `recvmsg_x`
/// the client is one `recv` per packet. **Default ON**; a syscall error latches
/// the scalar loop. `PUNKTFUNK_RECVMSG_X=0` forces the fallback.
#[cfg(target_vendor = "apple")]
mod recvx {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STATE: AtomicU8 = AtomicU8::new(0); // 0 = uninit, 1 = on, 2 = off

    pub fn active() -> bool {
        match STATE.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let on = std::env::var("PUNKTFUNK_RECVMSG_X")
                    .map(|v| v != "0")
                    .unwrap_or(true);
                STATE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
                on
            }
        }
    }
    pub fn disable() {
        STATE.store(2, Ordering::Relaxed);
    }
}

/// Darwin `struct msghdr_x` (`<sys/socket.h>`); `libc` does not expose it.
#[cfg(target_vendor = "apple")]
#[repr(C)]
struct MsghdrX {
    msg_name: *mut libc::c_void,
    msg_namelen: libc::socklen_t,
    msg_iov: *mut libc::iovec,
    msg_iovlen: libc::c_int,
    msg_control: *mut libc::c_void,
    msg_controllen: libc::socklen_t,
    msg_flags: libc::c_int,
    msg_datalen: libc::size_t,
}

// Hand-written Darwin `msghdr_x` (`libc` has none). A wrong offset hands the
// kernel a bad pointer or length. 32-bit fields pad before the following
// pointers — easy to get wrong silently.
#[cfg(target_vendor = "apple")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<MsghdrX>() == 56);
    assert!(offset_of!(MsghdrX, msg_name) == 0);
    assert!(offset_of!(MsghdrX, msg_namelen) == 8);
    assert!(offset_of!(MsghdrX, msg_iov) == 16); // 4 bytes of padding after msg_namelen
    assert!(offset_of!(MsghdrX, msg_iovlen) == 24);
    assert!(offset_of!(MsghdrX, msg_control) == 32); // padding after msg_iovlen
    assert!(offset_of!(MsghdrX, msg_controllen) == 40);
    assert!(offset_of!(MsghdrX, msg_flags) == 44);
    assert!(offset_of!(MsghdrX, msg_datalen) == 48);
};

#[cfg(target_vendor = "apple")]
unsafe extern "C" {
    /// Darwin batched receive: up to `cnt` datagrams in one syscall; returns the
    /// count and sets each `msg_datalen`. In libSystem on every macOS/iOS.
    fn recvmsg_x(
        s: libc::c_int,
        msgp: *mut MsghdrX,
        cnt: libc::c_uint,
        flags: libc::c_int,
    ) -> libc::ssize_t;
}

/// Batched receive via `recvmsg_x` into the caller's reused buffers.
/// SAFETY: each `MsghdrX` holds a raw pointer into `iovs`, which holds raw
/// pointers into `out`'s buffers; both `iovs` and `msgs` stay alive and
/// unmoved through the syscall.
#[cfg(target_vendor = "apple")]
fn recv_batch_x(
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
    let mut iovs: Vec<libc::iovec> = out[..n_bufs]
        .iter_mut()
        .map(|b| libc::iovec {
            iov_base: b.as_mut_ptr() as *mut libc::c_void,
            iov_len: b.len(),
        })
        .collect();
    let mut msgs: Vec<MsghdrX> = iovs
        .iter_mut()
        .map(|iov| {
            // SAFETY: MsghdrX is a plain-old-data libc-style struct; all-zeroes is its
            // documented "no ancillary data, no name" initial state.
            let mut m: MsghdrX = unsafe { std::mem::zeroed() };
            m.msg_iov = iov as *mut libc::iovec;
            m.msg_iovlen = 1;
            m
        })
        .collect();
    // SAFETY: `fd` is a live socket owned by `t`; `msgs` holds `n_bufs` initialized headers
    // whose iovecs point into `out`'s live buffers — both outlive the call.
    let n = unsafe {
        recvmsg_x(
            fd,
            msgs.as_mut_ptr(),
            n_bufs as libc::c_uint,
            libc::MSG_DONTWAIT,
        )
    };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if is_transient_io(&err) {
            return Ok(0);
        }
        return Err(err);
    }
    for (i, m) in msgs[..n as usize].iter().enumerate() {
        lens[i] = m.msg_datalen;
    }
    Ok(n as usize)
}

pub(super) fn recv_batch(
    t: &UdpTransport,
    out: &mut [Vec<u8>],
    lens: &mut [usize],
) -> std::io::Result<usize> {
    // Prefer `recvmsg_x` when enabled; a surprise error latches it off and
    // falls through to the scalar loop.
    #[cfg(target_vendor = "apple")]
    if recvx::active() {
        match recv_batch_x(t, out, lens) {
            Ok(n) => return Ok(n),
            Err(_) => recvx::disable(),
        }
    }
    let n_bufs = out.len().min(lens.len());
    let mut got = 0usize;
    // The socket is non-blocking, so an empty queue reads as `WouldBlock`.
    while got < n_bufs {
        match t.socket.recv(&mut out[got]) {
            Ok(n) => {
                lens[got] = n;
                got += 1;
            }
            Err(e) if is_transient_io(&e) => break, // drained or stale ICMP — no data this poll
            Err(_) if got > 0 => break, // keep what we have; the next empty poll surfaces it
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Block until `socket` has a datagram to read or `timeout` passes; `true` when readable. An
/// interrupted wait reads as a timeout: the caller loops anyway.
#[cfg(feature = "quic")]
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
