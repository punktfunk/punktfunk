//! What the media path still needs from raw UDP: [`wait_readable`] for the shared socket's
//! reader, which quinn-udp keeps non-blocking. The shared socket batches through quinn-udp
//! itself.

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
