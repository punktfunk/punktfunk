//! The supervisor's pad broker socket (`design/seat-pad-broker.md`): a running seat's host asks
//! here for a pad, and the supervisor builds and relays it (`pf_inject::pad_broker`).
//!
//! The socket file is world-connectable; the gate is `SO_PEERCRED`: a peer whose uid is not a
//! running seat's (the owner's row included) is closed unanswered. A seat holds at most
//! [`MAX_PER_SEAT`] pads; past that the answer is `capacity`. A pad lives as long as its relay
//! thread: the seat hanging up, or its host dying, ends both.

use pf_inject::pad_broker::{self, Status};
use pf_seats::linux::{socket, LinuxBackend};
use pf_seats::SeatService;
use std::collections::HashMap;
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

pub const SOCKET_PATH: &str = pf_paths::seat::PADS_SOCKET;
/// Relayed pads one seat may hold at once: its players' controllers, with room for a swap.
pub const MAX_PER_SEAT: usize = 8;
/// A seat sends its request at once; one that doesn't is not a seat host.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Pads held per uid, for the life of their relays.
static HELD: LazyLock<Mutex<HashMap<u32, usize>>> = LazyLock::new(Mutex::default);

/// One of `uid`'s pad slots, given back on drop.
struct Slot(u32);

impl Slot {
    fn take(uid: u32) -> Option<Slot> {
        admit(&mut HELD.lock().unwrap_or_else(|e| e.into_inner()), uid).then_some(Slot(uid))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        release(&mut HELD.lock().unwrap_or_else(|e| e.into_inner()), self.0);
    }
}

/// Counts one more pad for `uid`; `false` at the cap.
fn admit(held: &mut HashMap<u32, usize>, uid: u32) -> bool {
    let count = held.entry(uid).or_insert(0);
    if *count >= MAX_PER_SEAT {
        return false;
    }
    *count += 1;
    true
}

fn release(held: &mut HashMap<u32, usize>, uid: u32) {
    if let Some(count) = held.get_mut(&uid) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            held.remove(&uid);
        }
    }
}

/// Binds `path`, replacing a stale socket file, open to every local user: the peer's uid is
/// what admits a request.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let listener = socket::bind_fresh(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// Serves `listener` until the process ends: one thread per pad, for the pad's life.
pub fn serve(listener: UnixListener, service: Arc<SeatService<LinuxBackend>>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, "pads socket accept");
                continue;
            }
        };
        let service = Arc::clone(&service);
        let spawned = std::thread::Builder::new()
            .name("seat-pad".into())
            .spawn(move || handle(&service, stream));
        if let Err(error) = spawned {
            tracing::warn!(%error, "pads socket thread did not start");
        }
    }
}

fn handle(service: &SeatService<LinuxBackend>, mut stream: UnixStream) {
    let uid = match socket::peer_uid(&stream) {
        Ok(uid) => uid,
        Err(error) => {
            tracing::warn!(%error, "pads socket peer credentials");
            return;
        }
    };
    let Some(account) = service.backend().running_account(uid) else {
        tracing::warn!(uid, "pads socket refused a peer that is no running seat");
        return;
    };
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let request = match pad_broker::read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            tracing::debug!(%error, account, "pads socket request");
            return;
        }
    };
    let answer = |status: Status| {
        if let Err(error) = pad_broker::reply(&stream, status, None) {
            tracing::debug!(%error, account, "pads socket answer");
        }
    };
    let Some(_slot) = Slot::take(uid) else {
        tracing::warn!(
            account,
            cap = MAX_PER_SEAT,
            "seat asked for one pad too many"
        );
        answer(Status::Capacity);
        return;
    };
    let pad = match pad_broker::build(request.kind, request.index, &account) {
        Ok(pad) => pad,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), account, pad = request.kind.label(),
                "seat pad not made");
            answer(Status::Failed);
            return;
        }
    };
    let (ours, theirs) = match pad_broker::relay_pair() {
        Ok(pair) => pair,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), account, "seat pad relay not made");
            answer(Status::Failed);
            return;
        }
    };
    if let Err(error) = pad_broker::reply(&stream, Status::Created, Some(theirs.as_fd())) {
        tracing::warn!(error = %format!("{error:#}"), account, "seat pad not handed over");
        return;
    }
    drop(theirs);
    drop(stream);
    tracing::info!(
        account,
        pad = request.kind.label(),
        index = request.index,
        "seat pad made"
    );
    pad_broker::relay(pad, ours);
    tracing::info!(
        account,
        pad = request.kind.label(),
        index = request.index,
        "seat pad released"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seat_holds_at_most_the_cap_and_gets_slots_back() {
        let mut held = HashMap::new();
        for _ in 0..MAX_PER_SEAT {
            assert!(admit(&mut held, 987));
        }
        assert!(!admit(&mut held, 987), "one too many");
        assert!(admit(&mut held, 988), "another seat has its own cap");
        release(&mut held, 987);
        assert!(admit(&mut held, 987));
        for _ in 0..=MAX_PER_SEAT {
            release(&mut held, 987);
        }
        assert!(!held.contains_key(&987), "a seat with no pads is forgotten");
    }

    #[test]
    fn the_pads_socket_is_open_to_every_local_user() {
        let dir = std::env::temp_dir().join(format!("pf-pads-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pads.sock");
        let _listener = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o666);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
