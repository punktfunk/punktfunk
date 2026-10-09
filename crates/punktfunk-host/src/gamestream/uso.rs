//! Windows UDP Send Offload (`WSASendMsg` USO) for GameStream video: a paced burst's
//! equal-size packets leave in one syscall per 512. The Linux twin is `sendmmsg_all` in
//! [`super::stream`]. The batch logic builds on every OS so its tests run.

/// Process-wide UDP Send Offload. On by default; `PUNKTFUNK_GSO=0` kills it.
/// Support latches from the first send error, not a `setsockopt` probe — the
/// probe would set a socket-wide segment size and fragment larger plain `send`s.
#[cfg(target_os = "windows")]
mod state {
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
    // WSA_CMSG_LEN(4) and WSA_CMSG_SPACE(4).
    let cmsg_len = cmsgdata_align(hdr) + std::mem::size_of::<u32>();
    let space = cmsgdata_align(hdr + cmsghdr_align(std::mem::size_of::<u32>()));
    // SAFETY: `ctrl` holds `space` = WSA_CMSG_SPACE(4) bytes, so the header stores and the
    // 4-byte payload write stay inside it; `write_unaligned` because `WSA_CMSG_DATA` has no
    // alignment guarantee. `msg`, `data`, `ctrl` and `sent` are locals that outlive the
    // synchronous call (no OVERLAPPED); `data.buf` points into `buf`, which WSASendMsg only
    // reads for `buf.len()` bytes; `socket` is borrowed, so its handle stays open.
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

/// USO batch for a caller-owned connected socket. Uniform batches only
/// ([`uniform_segment`]), ≤512 segments per `WSASendMsg`. Returns packets sent that way
/// (`Ok(0)` if USO is off or sizes mix). An unsupported error latches USO off
/// process-wide; a full buffer returns the count so far.
#[cfg(target_os = "windows")]
pub(super) fn send_uso_all(
    socket: &std::net::UdpSocket,
    packets: &[&[u8]],
) -> std::io::Result<usize> {
    if packets.is_empty() || !state::active() {
        return Ok(0);
    }
    let Some(seg) = uniform_segment(packets) else {
        return Ok(0);
    };
    let send_one = |buf: &[u8], seg| send_one_uso(socket, buf, seg);
    match send_segmented(packets, seg, USO_MAX_SEGMENTS, send_one, uso_unsupported)? {
        Segmented::Sent(n) => Ok(n),
        Segmented::Unsupported(n) => {
            state::disable();
            Ok(n)
        }
    }
}

/// Lossy drop, not a failed send. `WouldBlock` is a full kernel buffer.
/// Connected-UDP `ConnectionRefused`/`ConnectionReset` are stale ICMP. Winsock's
/// no-buffer and unreachable/down codes have no stable `ErrorKind`, so they are
/// matched raw — same contract as `WouldBlock`.
fn is_transient_io(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, ConnectionReset, WouldBlock};
    if matches!(e.kind(), WouldBlock | ConnectionRefused | ConnectionReset) {
        return true;
    }
    // Winsock WSAE* raw codes (WSAEWOULDBLOCK already maps to WouldBlock).
    #[cfg(windows)]
    {
        matches!(
            e.raw_os_error(),
            Some(10055)   // WSAENOBUFS
                | Some(10051) // WSAENETUNREACH
                | Some(10065) // WSAEHOSTUNREACH
                | Some(10050) // WSAENETDOWN
                | Some(10064) // WSAEHOSTDOWN
        )
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The segment size of an offload batch: every packet `seg` bytes but the last, which may
/// be shorter. `None` for an empty or mixed batch, which goes out as plain datagrams.
fn uniform_segment(packets: &[&[u8]]) -> Option<usize> {
    let (last, rest) = packets.split_last()?;
    let seg = packets[0].len();
    (seg != 0 && rest.iter().all(|p| p.len() == seg) && last.len() <= seg).then_some(seg)
}

/// How a [`send_segmented`] run ended.
#[derive(Debug, PartialEq, Eq)]
enum Segmented {
    /// Packets sent. A transient error ends the run early; the caller owns the rest.
    Sent(usize),
    /// The path has no segment offload; this many packets went out before it said so.
    Unsupported(usize),
}

/// Coalesce a [`uniform_segment`] batch into `max_seg`-segment buffers and hand each to
/// `send_one` with the segment size. `unsupported` names the errors that mean this path
/// cannot offload; the caller latches that and picks the fallback.
fn send_segmented(
    packets: &[&[u8]],
    seg: usize,
    max_seg: usize,
    mut send_one: impl FnMut(&[u8], u16) -> std::io::Result<()>,
    unsupported: fn(&std::io::Error) -> bool,
) -> std::io::Result<Segmented> {
    let mut scratch: Vec<u8> = Vec::with_capacity(seg * packets.len().min(max_seg));
    let mut sent = 0usize;
    for chunk in packets.chunks(max_seg) {
        scratch.clear();
        for p in chunk {
            scratch.extend_from_slice(p);
        }
        match send_one(&scratch, seg as u16) {
            Ok(()) => sent += chunk.len(),
            Err(e) if is_transient_io(&e) => break,
            Err(e) if unsupported(&e) => return Ok(Segmented::Unsupported(sent)),
            Err(e) => return Err(e),
        }
    }
    Ok(Segmented::Sent(sent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_io_covers_connected_udp_blips() {
        use std::io::{Error, ErrorKind};
        for k in [
            ErrorKind::WouldBlock,
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
        ] {
            assert!(
                is_transient_io(&Error::from(k)),
                "{k:?} should be transient"
            );
        }
        for k in [ErrorKind::PermissionDenied, ErrorKind::AddrInUse] {
            assert!(!is_transient_io(&Error::from(k)), "{k:?} must stay fatal");
        }
    }

    #[test]
    fn uniform_segment_takes_a_short_last_packet_only() {
        let (a, b, c) = ([0u8; 3], [0u8; 2], [0u8; 4]);
        assert_eq!(uniform_segment(&[&a, &a, &b]), Some(3));
        assert_eq!(uniform_segment(&[&a]), Some(3));
        assert_eq!(uniform_segment(&[]), None);
        assert_eq!(uniform_segment(&[&a, &c]), None);
        assert_eq!(uniform_segment(&[&a, &b, &a]), None);
        assert_eq!(uniform_segment(&[&[]]), None);
    }

    /// Five 3-byte packets at two segments per send: 6 + 6 + 3 bytes, and the error
    /// ladder stops at the failing call.
    #[test]
    fn send_segmented_chunks_then_stops_on_error() {
        use std::io::{Error, ErrorKind};
        let p = [0u8; 3];
        let packets: [&[u8]; 5] = [&p; 5];
        let unsupported = |e: &Error| e.kind() == ErrorKind::Unsupported;
        let run = |fail_at: usize, kind: ErrorKind| {
            let mut calls = Vec::new();
            let r = send_segmented(
                &packets,
                3,
                2,
                |buf, seg| {
                    calls.push((buf.len(), seg));
                    if calls.len() == fail_at {
                        return Err(Error::from(kind));
                    }
                    Ok(())
                },
                unsupported,
            );
            (r.map_err(|e| e.kind()), calls)
        };
        let (r, calls) = run(0, ErrorKind::Other);
        assert_eq!(r, Ok(Segmented::Sent(5)));
        assert_eq!(calls, [(6, 3), (6, 3), (3, 3)]);
        assert_eq!(run(2, ErrorKind::WouldBlock).0, Ok(Segmented::Sent(2)));
        assert_eq!(
            run(2, ErrorKind::Unsupported).0,
            Ok(Segmented::Unsupported(2))
        );
        assert_eq!(
            run(2, ErrorKind::PermissionDenied).0,
            Err(ErrorKind::PermissionDenied)
        );
    }

    /// Winsock codes with no stable `ErrorKind` (they surface as `Uncategorized`).
    #[cfg(windows)]
    #[test]
    fn transient_io_covers_raw_tx_queue_and_path_codes() {
        use std::io::Error;
        // WSAENOBUFS / WSAENETUNREACH / WSAEHOSTUNREACH / WSAENETDOWN / WSAEHOSTDOWN.
        for code in [10055, 10051, 10065, 10050, 10064] {
            assert!(
                is_transient_io(&Error::from_raw_os_error(code)),
                "WSA code {code} should be transient"
            );
        }
        assert!(
            !is_transient_io(&Error::from_raw_os_error(10013)),
            "WSAEACCES must stay fatal"
        );
    }
}
