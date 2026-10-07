//! The supervisor's Unix socket, open to root and the `punktfunk` user only.
//!
//! One request per connection, framed as on the Windows pipe ([`crate::ipc::answer`]). The
//! socket file is `0660 root:punktfunk`, but the gate is the kernel's record of who connected
//! (`SO_PEERCRED`): a peer that is neither root nor the `punktfunk` user is closed unanswered. A
//! leftover socket from a crashed supervisor is replaced; a live one makes [`bind`] fail.

use super::accounts;
use crate::backend::PlatformBackend;
use crate::ipc::{ApiError, Command, CommandResult, ErrorCode, Request, Response};
use crate::service::SeatService;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Where the daemon listens and the box host connects.
pub const SOCKET_PATH: &str = "/run/punktfunk/seats.sock";

/// Requests served at once. A connection past it is closed unanswered.
const MAX_CONNECTIONS: usize = 8;
/// A client sends its frame at once; one that doesn't is not a client.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// A start takes up to 90 s on the server, so a client waits longer than that.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(180);

/// Root, or the `punktfunk` user when it exists. With no such user only root is let in.
pub fn peer_allowed(peer_uid: u32, punktfunk_uid: Option<u32>) -> bool {
    peer_uid == 0 || punktfunk_uid == Some(peer_uid)
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    Ok(rustix::net::sockopt::socket_peercred(stream)?.uid.as_raw())
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(_stream: &UnixStream) -> std::io::Result<u32> {
    Err(std::io::Error::other("peer credentials need Linux"))
}

/// Binds `path`, replacing a stale socket file. Fails when another supervisor answers on it.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match UnixStream::connect(path) {
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("another process answers on {}", path.display()),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => std::fs::remove_file(path)?,
    }
    let listener = UnixListener::bind(path)?;
    // The group may connect; peer credentials decide who is served. Without the group only root.
    let group = accounts::group_gid("punktfunk").ok().flatten();
    match group {
        Some(gid) => {
            std::os::unix::fs::chown(path, Some(0), Some(gid))?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
        }
        None => std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?,
    }
    Ok(listener)
}

/// Serves `listener` until the process ends. Each connection gets a thread that answers one
/// request.
pub fn serve<B: PlatformBackend>(listener: UnixListener, service: Arc<SeatService<B>>) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, "seats socket accept");
                continue;
            }
        };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!(
                limit = MAX_CONNECTIONS,
                "seats socket busy; connection closed"
            );
            continue;
        }
        let service = Arc::clone(&service);
        let counter = Arc::clone(&active);
        let spawned = std::thread::Builder::new()
            .name("seats-socket".into())
            .spawn(move || {
                handle(&service, stream);
                counter.fetch_sub(1, Ordering::SeqCst);
            });
        if let Err(error) = spawned {
            active.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!(%error, "seats socket thread did not start");
        }
    }
}

fn handle<B: PlatformBackend>(service: &SeatService<B>, mut stream: UnixStream) {
    let punktfunk = accounts::lookup("punktfunk").ok().flatten().map(|u| u.uid);
    match peer_uid(&stream) {
        Ok(uid) if peer_allowed(uid, punktfunk) => {}
        Ok(uid) => {
            tracing::warn!(uid, "seats socket refused a peer");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "seats socket peer credentials");
            return;
        }
    }
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    if let Err(error) = crate::ipc::answer(service, &mut stream) {
        tracing::debug!(%error, "seats socket request");
    }
}

/// Sends one request on `path` and reads its answer: the box host's side of the socket.
pub fn request(path: &Path, command: Command) -> Result<CommandResult, ApiError> {
    let transport = |what: &str, error: &dyn std::fmt::Display| {
        ApiError::new(ErrorCode::Transport, format!("{what}: {error}"))
    };
    let mut stream =
        UnixStream::connect(path).map_err(|e| transport("connect to the seats socket", &e))?;
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_read_timeout(Some(ANSWER_TIMEOUT));
    crate::ipc::write_json_frame(&mut stream, &Request::new(command))
        .map_err(|e| transport("write the seats request", &e))?;
    match crate::ipc::read_json_frame::<_, Response>(&mut stream)
        .map_err(|e| transport("read the seats answer", &e))?
    {
        Response::Success { result, .. } => Ok(result),
        Response::Error { error, .. } => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_the_punktfunk_user_are_served_and_nobody_else() {
        assert!(peer_allowed(0, None));
        assert!(peer_allowed(0, Some(972)));
        assert!(peer_allowed(972, Some(972)));
        assert!(!peer_allowed(972, None), "no punktfunk user: root only");
        assert!(!peer_allowed(1000, Some(972)));
        assert!(!peer_allowed(975, Some(972)), "a seat user is not the door");
    }

    /// A list over a real socket answers with the empty ledger, and a second bind on a live
    /// socket is refused. Peer credentials need Linux, so the exchange runs on the client side
    /// of `answer` only.
    #[test]
    fn a_second_supervisor_cannot_take_a_live_socket() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("seats.sock");
        let _first = bind(&path).unwrap();
        let second = bind(&path).unwrap_err();
        assert_eq!(second.kind(), std::io::ErrorKind::AddrInUse);
    }

    /// A file nobody listens on is replaced. A dead listener's socket file reads the same to
    /// `connect`; a plain file stands in for it because macOS can leak a just-closed listener
    /// into a concurrent `fork`.
    #[test]
    fn a_stale_socket_file_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("seats.sock");
        std::fs::write(&path, "").unwrap();
        bind(&path).unwrap();
    }
}
