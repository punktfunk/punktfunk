//! What the media path still needs from raw UDP: [`wait_readable`] for the shared socket's
//! reader, which quinn-udp keeps non-blocking, and Windows USO for GameStream video
//! ([`send_uso_all`]). The shared socket batches through quinn-udp itself.

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::send_uso_all;
#[cfg(target_os = "windows")]
#[cfg(feature = "quic")]
pub(crate) use windows::wait_readable;

/// Block until `socket` has a datagram to read or `timeout` passes; `true` when readable. An
/// interrupted wait reads as a timeout: the caller loops anyway.
#[cfg(all(unix, not(target_family = "wasm"), feature = "quic"))]
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

/// Lossy drop, not a failed send. `WouldBlock` is a full kernel buffer.
/// Connected-UDP `ConnectionRefused`/`ConnectionReset` are stale ICMP. `ENOBUFS`,
/// `WSAENOBUFS` (10055), and the `ENET*`/`EHOST*` family have no stable
/// `ErrorKind` (Rust maps them to `Uncategorized`), so they are matched as
/// raw errno below — same contract as `WouldBlock`.
#[cfg(any(target_os = "windows", test))]
fn is_transient_io(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, ConnectionReset, WouldBlock};
    if matches!(e.kind(), WouldBlock | ConnectionRefused | ConnectionReset) {
        return true;
    }
    // No stable `ErrorKind` for these; match the raw errno.
    #[cfg(unix)]
    {
        matches!(
            e.raw_os_error(),
            Some(libc::ENOBUFS)
                | Some(libc::ENETUNREACH)
                | Some(libc::EHOSTUNREACH)
                | Some(libc::ENETDOWN)
                | Some(libc::EHOSTDOWN)
        )
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
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// The segment size of an offload batch: every packet `seg` bytes but the last, which may
/// be shorter. `None` for an empty or mixed batch, which goes out as plain datagrams.
#[cfg(any(target_os = "windows", test))]
fn uniform_segment(packets: &[&[u8]]) -> Option<usize> {
    let (last, rest) = packets.split_last()?;
    let seg = packets[0].len();
    (seg != 0 && rest.iter().all(|p| p.len() == seg) && last.len() <= seg).then_some(seg)
}

/// How a [`send_segmented`] run ended.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, PartialEq, Eq)]
enum Segmented {
    /// Packets sent. A transient error ends the run early; the caller owns the rest.
    Sent(usize),
    /// The path has no segment offload; this many packets went out before it said so.
    Unsupported(usize),
}

/// Coalesce a [`uniform_segment`] batch into `max_seg`-segment buffers and hand each to
/// `send_one` (GSO or USO) with the segment size. `unsupported` names the errors that
/// mean this path cannot offload; the caller latches that and picks the fallback.
#[cfg(any(target_os = "windows", test))]
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

    /// Raw errno with no stable `ErrorKind` (they surface as `Uncategorized`).
    #[test]
    fn transient_io_covers_raw_tx_queue_and_path_codes() {
        use std::io::Error;

        #[cfg(unix)]
        {
            for code in [
                libc::ENOBUFS,
                libc::ENETUNREACH,
                libc::EHOSTUNREACH,
                libc::ENETDOWN,
                libc::EHOSTDOWN,
            ] {
                assert!(
                    is_transient_io(&Error::from_raw_os_error(code)),
                    "unix errno {code} should be transient"
                );
            }
            assert!(
                !is_transient_io(&Error::from_raw_os_error(libc::EACCES)),
                "EACCES must stay fatal"
            );
        }

        #[cfg(windows)]
        {
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
}
