//! Windows UDP Send Offload (`WSASendMsg` USO) for [`send_uso_all`], and the readable wait.

// `deny(unsafe_code)` carve-out (lib.rs): WSASendMsg USO on caller-owned buffers. Proofs at each site.
#![allow(unsafe_code)]

use super::Segmented;

/// Process-wide UDP Send Offload. On by default; `PUNKTFUNK_GSO=0` kills it.
/// Support latches from the first send error, not a `setsockopt` probe — the
/// probe would set a socket-wide segment size and fragment larger plain `send`s.
#[cfg(target_os = "windows")]
mod uso {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STATE: AtomicU8 = AtomicU8::new(0); // 0 = uninit, 1 = on, 2 = off

    pub fn active() -> bool {
        match STATE.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let off = std::env::var_os("PUNKTFUNK_GSO")
                    .map(|v| v == "0")
                    .unwrap_or(false);
                STATE.store(if off { 2 } else { 1 }, Ordering::Relaxed);
                tracing::info!(
                    enabled = !off,
                    "Windows UDP Send Offload (USO) resolved (the 1 Gbps+ send lever; PUNKTFUNK_GSO=0 disables)"
                );
                !off
            }
        }
    }
    /// Latch USO off process-wide after a send that means this path cannot use it.
    pub fn disable() {
        if STATE.swap(2, Ordering::Relaxed) != 2 {
            tracing::warn!(
                "Windows USO unsupported on this path — falling back to per-packet sends"
            );
        }
    }
}

/// `WSASendMsg` segment cap; more fail the syscall.
#[cfg(target_os = "windows")]
const USO_MAX_SEGMENTS: usize = 512;

/// `WSASendMsg` errors that mean USO is unusable here (not a transient `WouldBlock`).
/// 10022 WSAEINVAL, 10042 WSAENOPROTOOPT, 10045 WSAEOPNOTSUPP, 10040 WSAEMSGSIZE.
#[cfg(target_os = "windows")]
fn uso_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(10022) | Some(10042) | Some(10045) | Some(10040)
    )
}

/// `WSA_CMSG_*` are C macros the `windows` crate does not export, so the cmsg
/// layout is reimplemented here.
#[cfg(target_os = "windows")]
fn send_one_uso(socket: &std::net::UdpSocket, buf: &[u8], seg_size: u16) -> std::io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        WSASendMsg, CMSGHDR, IPPROTO_UDP, UDP_SEND_MSG_SIZE, WSABUF, WSAMSG,
    };
    let align_usize = std::mem::align_of::<usize>();
    let align_hdr = std::mem::align_of::<CMSGHDR>();
    let cmsgdata_align = |n: usize| (n + align_usize - 1) & !(align_usize - 1);
    let cmsghdr_align = |n: usize| (n + align_hdr - 1) & !(align_hdr - 1);
    let hdr = std::mem::size_of::<CMSGHDR>();

    // 8-byte-aligned control buffer; 32 B holds one u32 cmsg (WSA_CMSG_SPACE(4) = 24 on x64).
    #[repr(align(8))]
    struct Aligned([u8; 32]);
    let mut ctrl = Aligned([0u8; 32]);

    let mut data = WSABUF {
        len: buf.len() as u32,
        buf: buf.as_ptr() as *mut u8, // WSASendMsg only reads it
    };
    let mut msg = WSAMSG {
        name: std::ptr::null_mut(),
        namelen: 0,
        lpBuffers: &mut data,
        dwBufferCount: 1,
        Control: WSABUF {
            len: 0,
            buf: ctrl.0.as_mut_ptr(),
        },
        dwFlags: 0,
    };
    let cmsg_len = cmsgdata_align(hdr) + std::mem::size_of::<u32>(); // WSA_CMSG_LEN(4)
    let space = cmsgdata_align(hdr + cmsghdr_align(std::mem::size_of::<u32>())); // WSA_CMSG_SPACE(4)
                                                                                 // SAFETY: `ctrl` is a local control buffer sized by `WSA_CMSG_SPACE(4)` — computed as `space`
                                                                                 // just above — so the header plus its 4-byte payload fit inside it and neither the field stores
                                                                                 // nor the unaligned data write can run past the end. `write_unaligned` is used because the
                                                                                 // payload sits at a `WSA_CMSG_DATA` offset with no alignment guarantee.
    unsafe {
        let cmsg = ctrl.0.as_mut_ptr() as *mut CMSGHDR;
        (*cmsg).cmsg_len = cmsg_len;
        (*cmsg).cmsg_level = IPPROTO_UDP;
        (*cmsg).cmsg_type = UDP_SEND_MSG_SIZE;
        let data_ptr = (cmsg as usize + cmsgdata_align(hdr)) as *mut u32;
        std::ptr::write_unaligned(data_ptr, seg_size as u32);
        msg.Control.len = space as u32;
        let mut sent = 0u32;
        let rc = WSASendMsg(
            socket.as_raw_socket() as usize,
            &msg,
            0,
            &mut sent,
            std::ptr::null_mut(),
            None,
        );
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// USO batch for a caller-owned connected socket (GameStream video).
/// Uniform batches only ([`super::uniform_segment`]), ≤512 segments per `WSASendMsg`.
/// Returns packets sent that way (`Ok(0)` if USO is off or sizes mix). An unsupported
/// error latches USO off process-wide; a full buffer returns the count so far.
#[cfg(target_os = "windows")]
pub fn send_uso_all(socket: &std::net::UdpSocket, packets: &[&[u8]]) -> std::io::Result<usize> {
    if packets.is_empty() || !uso::active() {
        return Ok(0);
    }
    let Some(seg) = super::uniform_segment(packets) else {
        return Ok(0);
    };
    let send_one = |buf: &[u8], seg| send_one_uso(socket, buf, seg);
    match super::send_segmented(packets, seg, USO_MAX_SEGMENTS, send_one, uso_unsupported)? {
        Segmented::Sent(n) => Ok(n),
        Segmented::Unsupported(n) => {
            uso::disable();
            Ok(n)
        }
    }
}

/// Block until `socket` has a datagram to read or `timeout` passes; `true` when readable.
#[cfg(feature = "quic")]
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
