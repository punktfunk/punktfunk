//! The supervisor's Unix socket, open to root and the `punktfunk` user, and to the box owner for
//! box-wide Flatpaks.
//!
//! One request per connection, framed as on the Windows pipe ([`crate::ipc::answer`]). The
//! socket file is `0660 root:punktfunk-door`, the door user's own group, plus an ACL entry for
//! the box owner's uid, but the gate is the kernel's record of who connected (`SO_PEERCRED`): a
//! peer [`command_allowed`] refuses is closed unanswered. A leftover socket from a crashed
//! supervisor is replaced; a live one makes [`bind`] fail.

use super::shared::{
    acl_bytes, ACL_ACCESS, ACL_GROUP_OBJ, ACL_MASK, ACL_NO_ID, ACL_OTHER, ACL_USER, ACL_USER_OBJ,
};
use super::{accounts, LinuxBackend};
use crate::ipc::{ApiError, Command, CommandResult, ErrorCode, Request, Response};
use crate::service::SeatService;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where the daemon listens and the box host connects.
pub const SOCKET_PATH: &str = "/run/punktfunk/seats.sock";

/// The door user's primary group, which reads this socket and the box's library. Not
/// `punktfunk`, the group of the vhci attach nodes.
pub const DOOR_GROUP: &str = "punktfunk-door";

/// Requests served at once. A connection past it is closed unanswered.
const MAX_CONNECTIONS: usize = 8;
/// A client sends its frame at once; one that doesn't is not a client.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// A start takes up to 90 s on the server, so a client waits longer than that.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(180);
/// A Flatpak install downloads hundreds of megabytes.
const FLATPAK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Root, or the `punktfunk` user when it exists. With no such user only root is let in.
pub fn peer_allowed(peer_uid: u32, punktfunk_uid: Option<u32>) -> bool {
    peer_uid == 0 || punktfunk_uid == Some(peer_uid)
}

/// Whether `peer_uid` may send `command`: anything as a peer [`peer_allowed`] lets in, a
/// box-wide Flatpak change as the box owner `owner_uid`, nothing otherwise.
pub fn command_allowed(
    peer_uid: u32,
    punktfunk_uid: Option<u32>,
    owner_uid: Option<u32>,
    command: &Command,
) -> bool {
    peer_allowed(peer_uid, punktfunk_uid)
        || (owner_uid == Some(peer_uid)
            && matches!(
                command,
                Command::InstallFlatpak { .. } | Command::RemoveFlatpak { .. }
            ))
}

/// The uid behind `stream` (`SO_PEERCRED`): what admits a request here and on the pad broker.
#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    Ok(rustix::net::sockopt::socket_peercred(stream)?.uid.as_raw())
}

#[cfg(not(target_os = "linux"))]
pub fn peer_uid(_stream: &UnixStream) -> std::io::Result<u32> {
    Err(std::io::Error::other("peer credentials need Linux"))
}

/// Binds `path`, replacing a stale socket file. Fails when another process answers on it.
/// The file keeps the default mode; the caller sets who may connect.
pub fn bind_fresh(path: &Path) -> std::io::Result<UnixListener> {
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
    UnixListener::bind(path)
}

/// Makes the door's group when no package has yet. A package restarts this daemon before the one
/// that creates the group, and a socket bound without it stays closed to the door.
pub fn ensure_door_group() {
    if let Err(error) = accounts::ensure_group(DOOR_GROUP) {
        tracing::warn!(%error, "door group not created");
    }
}

/// Binds `path` for the box host: root and the door's group may connect.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let listener = bind_fresh(path)?;
    // The group may connect; peer credentials decide who is served. Without the group only root.
    let group = accounts::group_gid(DOOR_GROUP).ok().flatten();
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
/// request, then lets the box owner's uid connect if the owner changed.
pub fn serve(listener: UnixListener, service: Arc<SeatService<LinuxBackend>>) {
    let path = listener.local_addr().ok();
    let acl = Arc::new(OwnerAcl {
        path: path.and_then(|a| a.as_pathname().map(Path::to_path_buf)),
        admitted: Mutex::new(None),
    });
    acl.follow(service.backend().owner_uid());
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
        let acl = Arc::clone(&acl);
        let spawned = std::thread::Builder::new()
            .name("seats-socket".into())
            .spawn(move || {
                handle(&service, stream);
                acl.follow(service.backend().owner_uid());
                counter.fetch_sub(1, Ordering::SeqCst);
            });
        if let Err(error) = spawned {
            active.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!(%error, "seats socket thread did not start");
        }
    }
}

fn handle(service: &SeatService<LinuxBackend>, mut stream: UnixStream) {
    let punktfunk = accounts::lookup("punktfunk").ok().flatten().map(|u| u.uid);
    let owner = service.backend().owner_uid();
    let uid = match peer_uid(&stream) {
        Ok(uid) if peer_allowed(uid, punktfunk) || owner == Some(uid) => uid,
        Ok(uid) => {
            tracing::warn!(uid, "seats socket refused a peer");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "seats socket peer credentials");
            return;
        }
    };
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let admit = |command: &Command| {
        let allowed = command_allowed(uid, punktfunk, owner, command);
        if !allowed {
            tracing::warn!(uid, "seats socket refused the box owner a command");
        }
        allowed
    };
    if let Err(error) = crate::ipc::answer_if(service, &mut stream, admit) {
        tracing::debug!(%error, "seats socket request");
    }
}

/// The socket file and the owner uid its ACL admits: `None` until set, then that uid, if any.
struct OwnerAcl {
    path: Option<PathBuf>,
    admitted: Mutex<Option<Option<u32>>>,
}

impl OwnerAcl {
    /// Lets `owner`'s uid connect, and no earlier owner's, when that isn't what the file admits
    /// already. The owner's own uid is not in the door's group, which the file's mode opens to.
    fn follow(&self, owner: Option<u32>) {
        let mut admitted = self.admitted.lock().unwrap_or_else(|e| e.into_inner());
        let Some(path) = self.path.as_deref().filter(|_| *admitted != Some(owner)) else {
            return;
        };
        let set = || -> std::io::Result<()> {
            let door = accounts::group_gid(DOOR_GROUP).ok().flatten();
            let group = if door == Some(std::fs::symlink_metadata(path)?.gid()) {
                6
            } else {
                0
            };
            let acl = socket_acl(group, owner);
            Ok(rustix::fs::lsetxattr(
                path,
                ACL_ACCESS,
                &acl,
                rustix::fs::XattrFlags::empty(),
            )?)
        };
        match set() {
            Ok(()) => *admitted = Some(owner),
            Err(error) => {
                tracing::warn!(%error, ?owner, "seats socket not opened to the box owner")
            }
        }
    }
}

/// The socket file's access ACL as the kernel stores it: the file's user `rw`, its group
/// `group`, others nothing and, with `owner`, that uid `rw` under an `rw` mask. Without one it
/// is the bare mode, which the kernel keeps as the mode alone.
fn socket_acl(group: u16, owner: Option<u32>) -> Vec<u8> {
    let mut entries = vec![(ACL_USER_OBJ, 6, ACL_NO_ID)];
    entries.extend(owner.map(|uid| (ACL_USER, 6, uid)));
    entries.push((ACL_GROUP_OBJ, group, ACL_NO_ID));
    if owner.is_some() {
        entries.push((ACL_MASK, 6, ACL_NO_ID));
    }
    entries.push((ACL_OTHER, 0, ACL_NO_ID));
    acl_bytes(&entries)
}

/// How long the box host waits for `command`'s answer.
fn answer_within(command: &Command) -> Duration {
    match command {
        Command::InstallFlatpak { .. } | Command::RemoveFlatpak { .. } => FLATPAK_TIMEOUT,
        _ => ANSWER_TIMEOUT,
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
    let _ = stream.set_read_timeout(Some(answer_within(&command)));
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

    /// The box owner may ask for box-wide Flatpaks and nothing else; root and the door may ask
    /// for anything; a seat user, or an owner while no owner row is prepared, for nothing.
    #[test]
    fn the_box_owner_may_change_flatpaks_and_nothing_else() {
        let install = Command::InstallFlatpak {
            id: "net.pcsx2.PCSX2".into(),
        };
        let remove = Command::RemoveFlatpak {
            id: "net.pcsx2.PCSX2".into(),
            purge: false,
        };
        let others = [
            Command::List,
            Command::Doctor,
            Command::AdoptOwner {
                account: "enrico".into(),
            },
            Command::Delete {
                id: crate::model::SeatId::parse("01".repeat(16)).unwrap(),
            },
        ];
        let (door, owner) = (Some(972), Some(1000));
        for command in [&install, &remove] {
            assert!(command_allowed(1000, door, owner, command));
            assert!(command_allowed(0, door, owner, command));
            assert!(command_allowed(972, door, owner, command));
            assert!(!command_allowed(1000, door, None, command), "no owner row");
            assert!(!command_allowed(975, door, owner, command), "a seat user");
        }
        for command in &others {
            assert!(!command_allowed(1000, door, owner, command), "{command:?}");
            assert!(command_allowed(0, door, owner, command));
            assert!(command_allowed(972, door, owner, command));
        }
    }

    /// The owner's entry sits between the file's user and group entries, under an `rw` mask;
    /// without an owner the ACL is the bare mode, which drops an earlier owner's entry.
    #[test]
    fn the_socket_acl_names_the_owner_alone() {
        let entry = |tag: u16, perm: u16, id: u32| {
            [
                &tag.to_le_bytes()[..],
                &perm.to_le_bytes(),
                &id.to_le_bytes(),
            ]
            .concat()
        };
        let none = u32::MAX;
        let version = 2u32.to_le_bytes().to_vec();
        assert_eq!(
            socket_acl(6, Some(1000)),
            [
                version.clone(),
                entry(0x01, 6, none),
                entry(0x02, 6, 1000),
                entry(0x04, 6, none),
                entry(0x10, 6, none),
                entry(0x20, 0, none),
            ]
            .concat()
        );
        assert_eq!(
            socket_acl(0, None),
            [
                version,
                entry(0x01, 6, none),
                entry(0x04, 0, none),
                entry(0x20, 0, none),
            ]
            .concat()
        );
    }

    /// A new owner's uid gets the socket file's ACL entry, and losing the owner drops it and
    /// leaves the mode `bind` set. POSIX ACLs are Linux's.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_socket_file_follows_the_owner() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("seats.sock");
        let _listener = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let acl = OwnerAcl {
            path: Some(path.clone()),
            admitted: Mutex::new(None),
        };
        acl.follow(Some(4242));
        let mut buf = vec![0u8; 256];
        let Ok(len) = rustix::fs::getxattr(&path, ACL_ACCESS, &mut buf[..]) else {
            eprintln!("no POSIX ACLs under {}: skipped", temp.path().display());
            return;
        };
        let group = ((mode >> 3) & 7) as u16;
        assert_eq!(&buf[..len], socket_acl(group, Some(4242)).as_slice());
        acl.follow(None);
        assert!(rustix::fs::getxattr(&path, ACL_ACCESS, &mut buf[..]).is_err());
        let after = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(after, mode);
    }

    #[test]
    fn a_flatpak_change_is_waited_on_longer_than_a_start() {
        assert_eq!(answer_within(&Command::List), ANSWER_TIMEOUT);
        let install = Command::InstallFlatpak { id: "a.b".into() };
        assert_eq!(answer_within(&install), FLATPAK_TIMEOUT);
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
