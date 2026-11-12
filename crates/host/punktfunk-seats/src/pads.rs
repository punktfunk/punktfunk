//! The supervisor's pad broker socket (`design/seat-pad-broker.md`): a running seat's host asks
//! here for a pad, and the supervisor builds and relays it (`pf_inject::pad_broker`).
//!
//! The socket file is world-connectable; the gate is `SO_PEERCRED`: a peer whose uid is not a
//! running seat's (the owner's row included) is closed unanswered. A seat holds at most
//! [`MAX_PER_SEAT`] pads and [`vhci::MAX_PER_SEAT`] USB devices; past that the answer is
//! `capacity`. A USB device is served by a pad server of its own, in a scope of its own
//! ([`serve_usb`]), so no byte of its descriptors comes from the seat and a supervisor restart
//! leaves it running. A pad lives as long as its relay, a USB device as long as its pad server:
//! the seat hanging up, or its host dying, ends both. A relayed pad and an attached USB socket
//! sit in systemd's fd store, so a supervisor restart picks them up again ([`resume`]). A seat's
//! stop takes both back ([`revoke`]).

use crate::vhci;
use anyhow::Context as _;
use pf_inject::pad_broker::{self, BrokeredPad, PadKind, Request, Status};
use pf_inject::usbip;
use pf_inject::usbip_relay::Hello;
use pf_seats::linux::{socket, LinuxBackend};
use pf_seats::SeatService;
use rustix::net::Shutdown;
use std::collections::HashMap;
use std::net::TcpStream;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

pub const SOCKET_PATH: &str = pf_paths::seat::PADS_SOCKET;
/// Relayed pads one seat may hold at once: its players' controllers, with room for a swap.
pub const MAX_PER_SEAT: usize = 8;
/// A seat sends its request at once; one that doesn't is not a seat host.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a new pad server has to answer the import: `systemd-run` and an exec first.
const PAD_SERVER_WAIT: Duration = Duration::from_secs(5);
/// One file per relayed pad, `<account> <index> <kind>`, for `punktfunk-seats list`.
const RELAYED: &str = "/run/punktfunk/pads/relayed";

/// Pads held per account, for the life of their relays.
static HELD: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(Mutex::default);
/// Pad servers per account, from the request to the device's end.
static SERVED: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(Mutex::default);
/// Connections per peer uid, from accept until their request is read.
static PENDING: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(Mutex::default);
/// Unread connections one uid may hold: a seat host sends its request at once.
const PENDING_PER_UID: usize = 4;
/// Unread connections from every uid together, each a thread of the unit's own task limit.
const PENDING_TOTAL: usize = 16;

/// One of a key's slots in `held`, an account's or a peer uid's, given back on drop.
struct Slot {
    held: &'static Mutex<HashMap<String, usize>>,
    key: String,
}

impl Slot {
    fn take(held: &'static Mutex<HashMap<String, usize>>, key: &str, cap: usize) -> Option<Slot> {
        admit(
            &mut held.lock().unwrap_or_else(|e| e.into_inner()),
            key,
            cap,
        )
        .then(|| Slot {
            held,
            key: key.to_owned(),
        })
    }

    /// A slot for one unread connection from `uid`, within [`PENDING_PER_UID`] and
    /// [`PENDING_TOTAL`].
    fn pending(uid: u32) -> Option<Slot> {
        let key = uid.to_string();
        let admitted = admit_pending(&mut PENDING.lock().unwrap_or_else(|e| e.into_inner()), &key);
        admitted.then_some(Slot {
            held: &PENDING,
            key,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        release(
            &mut self.held.lock().unwrap_or_else(|e| e.into_inner()),
            &self.key,
        );
    }
}

/// Counts one more unread connection from `uid`; `false` at its cap or the total.
fn admit_pending(pending: &mut HashMap<String, usize>, uid: &str) -> bool {
    pending.values().sum::<usize>() < PENDING_TOTAL && admit(pending, uid, PENDING_PER_UID)
}

/// Counts one more for `account`; `false` at `cap`.
fn admit(held: &mut HashMap<String, usize>, account: &str, cap: usize) -> bool {
    let count = held.entry(account.to_owned()).or_insert(0);
    if *count >= cap {
        return false;
    }
    *count += 1;
    true
}

fn release(held: &mut HashMap<String, usize>, account: &str) {
    if let Some(count) = held.get_mut(account) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            held.remove(account);
        }
    }
}

/// A copy of every relay and USB socket a seat holds, with its account, by [`Hold`] id.
static HOLDS: LazyLock<Mutex<HashMap<u64, (String, OwnedFd)>>> = LazyLock::new(Mutex::default);
static NEXT_HOLD: AtomicU64 = AtomicU64::new(0);

fn holds() -> MutexGuard<'static, HashMap<u64, (String, OwnedFd)>> {
    HOLDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// One socket `account` holds, listed for [`revoke`] until dropped.
struct Hold(u64);

impl Hold {
    fn new(account: &str, sock: BorrowedFd<'_>) -> std::io::Result<Hold> {
        let copy = sock.try_clone_to_owned()?;
        let id = NEXT_HOLD.fetch_add(1, Ordering::Relaxed);
        holds().insert(id, (account.to_owned(), copy));
        Ok(Hold(id))
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        holds().remove(&self.0);
    }
}

/// Takes back what `account` holds once its seat stopped: each relay and USB socket is shut
/// down, so its relay reads the seat hanging up and drops the pad, and `vhci_hcd` drops the USB
/// device, whose pad server then exits. A process that outlived the seat keeps none of them.
pub fn revoke(account: &str) {
    let held = holds();
    let mine: Vec<&OwnedFd> = held
        .values()
        .filter(|(holder, _)| holder == account)
        .map(|(_, sock)| sock)
        .collect();
    for sock in &mine {
        if let Err(error) = rustix::net::shutdown(sock, Shutdown::Both) {
            tracing::debug!(%error, account, "seat socket not shut down");
        }
    }
    if !mine.is_empty() {
        tracing::info!(account, sockets = mine.len(), "seat's pads taken back");
    }
}

/// Relays again every pad systemd kept across a supervisor restart, and watches again every
/// served USB socket. The other kept fds are the caller's. First thing at start: until then
/// the kept fds would leak into a child.
pub fn resume() -> Vec<(String, OwnedFd)> {
    let _ = std::fs::remove_dir_all(RELAYED);
    let pad_broker::Kept {
        pads,
        usb: sockets,
        other,
    } = pad_broker::kept();
    for kept in pads {
        let Some(slot) = Slot::take(&HELD, &kept.account, MAX_PER_SEAT) else {
            pad_broker::forget(&kept.name);
            continue;
        };
        tracing::info!(
            account = kept.account,
            pad = kept.kind.label(),
            index = kept.index,
            "seat pad kept across a restart"
        );
        let spawned = std::thread::Builder::new()
            .name("seat-pad".into())
            .spawn(move || {
                let _slot = slot;
                run_relay(
                    &kept.name,
                    &kept.account,
                    kept.kind,
                    kept.index,
                    kept.pad,
                    kept.relay,
                );
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "kept seat pad thread did not start");
        }
    }
    for (port, recorded, sock) in sockets {
        tracing::info!(port, "seat USB device kept across a restart");
        let spawned = std::thread::Builder::new()
            .name("seat-usb".into())
            .spawn(move || {
                watch_usb(port, recorded, sock);
                tracing::info!(port, "seat USB device gone");
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, port, "kept seat USB thread did not start");
        }
    }
    other
}

/// Relays `pad` until its seat hangs up or stops ([`revoke`]), listed for `list` and in
/// systemd's fd store meanwhile. A pad a stop couldn't take back is dropped at once.
fn run_relay(
    name: &str,
    account: &str,
    kind: PadKind,
    index: u8,
    pad: BrokeredPad,
    relay: UnixDatagram,
) {
    let _hold = match Hold::new(account, relay.as_fd()) {
        Ok(hold) => hold,
        Err(error) => {
            tracing::warn!(%error, account, pad = kind.label(), "seat pad not revocable, dropped");
            pad_broker::forget(name);
            return;
        }
    };
    let record =
        pad_broker::parse_store_name(name).map(|(serial, ..)| Path::new(RELAYED).join(serial));
    if let Some(record) = &record {
        let written = std::fs::create_dir_all(RELAYED)
            .and_then(|()| std::fs::write(record, format!("{account} {index} {}\n", kind as u8)));
        if let Err(error) = written {
            tracing::debug!(%error, "seat pad not listed");
        }
    }
    pad_broker::relay(pad, relay);
    pad_broker::forget(name);
    if let Some(record) = &record {
        let _ = std::fs::remove_file(record);
    }
    tracing::info!(account, pad = kind.label(), index, "seat pad released");
}

/// `list`'s lines for what the seats hold: `pad <account> <kind> #<index>` and
/// `usb <account> port <n>`.
pub fn listing() -> Vec<String> {
    let rows = |dir: &str| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                Some((
                    e.file_name().into_string().ok()?,
                    std::fs::read_to_string(e.path()).ok()?,
                ))
            })
            .collect::<Vec<_>>()
    };
    let mut out: Vec<String> = rows(RELAYED)
        .into_iter()
        .filter_map(|(_, row)| {
            let mut words = row.split_whitespace();
            let (account, index) = (words.next()?, words.next()?);
            let kind = PadKind::from_wire(words.next()?.parse().ok()?)?;
            Some(format!("pad {account} {} #{index}", kind.label()))
        })
        .collect();
    out.extend(rows(vhci::MAP).into_iter().filter_map(|(port, row)| {
        Some(format!(
            "usb {} port {port}",
            row.split_whitespace().next()?
        ))
    }));
    out.sort();
    out
}

/// Binds `path`, replacing a stale socket file, open to every local user: the peer's uid is
/// what admits a request.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let listener = socket::bind_fresh(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// At the supervisor's stop (`ungate`): without the socket, the box's own hidraw and joystick
/// nodes open to everyone again, as the distro's rules have them.
pub fn unbind() {
    let _ = std::fs::remove_file(SOCKET_PATH);
    refresh_pad_nodes();
}

/// Has udev apply its rules to every hidraw and `js*` node again. `65-punktfunk-seats.rules`
/// closes the box's own nodes to other users while the pads socket exists; a node made before
/// that keeps what it got until an event reaches it.
pub fn refresh_pad_nodes() {
    for (class, prefix) in [("/sys/class/hidraw", "hidraw"), ("/sys/class/input", "js")] {
        let Ok(entries) = std::fs::read_dir(class) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(prefix) {
                continue;
            }
            if let Err(error) = std::fs::write(entry.path().join("uevent"), "change") {
                tracing::warn!(%error, node = %name, "pad node not refreshed");
            }
        }
    }
}

/// Serves `listener` until the process ends: one thread per pad, for the pad's life. A
/// connection past its uid's share of unread ones ([`Slot::pending`]) closes unanswered.
pub fn serve(listener: UnixListener, service: Arc<SeatService<LinuxBackend>>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, "pads socket accept");
                continue;
            }
        };
        let uid = match socket::peer_uid(&stream) {
            Ok(uid) => uid,
            Err(error) => {
                tracing::warn!(%error, "pads socket peer credentials");
                continue;
            }
        };
        let Some(pending) = Slot::pending(uid) else {
            tracing::debug!(uid, "pads socket busy: connection closed");
            continue;
        };
        let service = Arc::clone(&service);
        let spawned = std::thread::Builder::new()
            .name("seat-pad".into())
            .spawn(move || handle(&service, stream, uid, pending));
        if let Err(error) = spawned {
            tracing::warn!(%error, "pads socket thread did not start");
        }
    }
}

/// Answers `uid`'s request. `pending` is given back once the request is read.
fn handle(service: &SeatService<LinuxBackend>, stream: UnixStream, uid: u32, pending: Slot) {
    let Some(account) = service.backend().running_account(uid) else {
        tracing::warn!(uid, "pads socket refused a peer that is no running seat");
        return;
    };
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let request = match pad_broker::read_request(&stream) {
        Ok(read) => read,
        Err(error) => {
            tracing::debug!(error = %format!("{error:#}"), account, "pads socket request");
            return;
        }
    };
    match request {
        Request::Create { kind, index } => {
            drop(pending);
            let seat = ps_seat(&service.ledger(), &account);
            create(stream, &account, kind, index, seat);
        }
        Request::Serve {
            kind,
            index,
            slot,
            identity_len,
        } => {
            let identity = pad_broker::read_identity(&stream, identity_len);
            drop(pending);
            match identity {
                Ok(identity) => {
                    let hello = Hello {
                        kind,
                        index,
                        slot,
                        seat: ps_seat(&service.ledger(), &account),
                        identity,
                    };
                    serve_usb(stream, &account, &hello);
                }
                Err(error) => {
                    tracing::debug!(error = %format!("{error:#}"), account, "pads socket request");
                }
            }
        }
    }
}

/// The seat `account`'s PlayStation pads carry in their MAC, as its own host marks them
/// ([`pad_broker::own_ps_seat`]): the owner's row keeps the box's.
fn ps_seat(ledger: &pf_seats::Ledger, account: &str) -> u16 {
    ledger
        .seats
        .iter()
        .find(|seat| seat.account == account && !seat.owner)
        .map_or(0, |seat| pad_broker::ps_seat(seat.id.as_str()))
}

fn answer(stream: &UnixStream, account: &str, status: Status) {
    if let Err(error) = pad_broker::reply(stream, status, 0, None) {
        tracing::debug!(error = %format!("{error:#}"), account, "pads socket answer");
    }
}

/// A uinput or uhid pad, relayed until the seat hangs up. A PlayStation pad's MAC carries `seat`.
fn create(stream: UnixStream, account: &str, kind: PadKind, index: u8, seat: u16) {
    let Some(_slot) = Slot::take(&HELD, account, MAX_PER_SEAT) else {
        tracing::warn!(
            account,
            cap = MAX_PER_SEAT,
            "seat asked for one pad too many"
        );
        answer(&stream, account, Status::Capacity);
        return;
    };
    let pad = match pad_broker::build(kind, index, account, seat) {
        Ok(pad) => pad,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), account, pad = kind.label(),
                "seat pad not made");
            answer(&stream, account, Status::Failed);
            return;
        }
    };
    let (ours, theirs) = match pad_broker::relay_pair() {
        Ok(pair) => pair,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), account, "seat pad relay not made");
            answer(&stream, account, Status::Failed);
            return;
        }
    };
    if let Err(error) = pad_broker::reply(&stream, Status::Created, 0, Some(theirs.as_fd())) {
        tracing::warn!(error = %format!("{error:#}"), account, "seat pad not handed over");
        return;
    }
    drop(theirs);
    drop(stream);
    tracing::info!(account, pad = kind.label(), index, "seat pad made");
    let name = pad_broker::store_name(kind, index, account);
    pad_broker::keep(&name, &pad, &ours);
    run_relay(&name, account, kind, index, pad, ours);
}

/// A seat's USB device, served by a pad server of its own: the device, its descriptors and
/// every control transfer come from pf-inject's tables, and the seat gets only the relay.
/// Held until the kernel's connection ends.
fn serve_usb(stream: UnixStream, account: &str, hello: &Hello) {
    let label = hello.kind.label();
    let Some(_slot) = Slot::take(&SERVED, account, vhci::MAX_PER_SEAT) else {
        tracing::warn!(
            account,
            cap = vhci::MAX_PER_SEAT,
            "seat asked for one USB device too many"
        );
        answer(&stream, account, Status::Capacity);
        return;
    };
    if let Err(error) = hello.identity() {
        tracing::warn!(error = %format!("{error:#}"), account, pad = label, "seat USB pad identity refused");
        answer(&stream, account, Status::Failed);
        return;
    }
    let (mut client, relay, mut server) = match start_pad_server(account, hello) {
        Ok(started) => started,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), account, pad = label, "seat USB pad server not started");
            answer(&stream, account, Status::Failed);
            return;
        }
    };
    let attached = usbip::import_handshake(&mut client, PAD_SERVER_WAIT)
        .and_then(|(devid, speed)| vhci::attach(account, client.as_fd(), devid, speed));
    let port = match attached {
        Ok(Some(port)) => port,
        failed => {
            let status = match failed {
                Ok(_) => Status::Capacity,
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), account, pad = label, "seat USB device not attached");
                    Status::Failed
                }
            };
            answer(&stream, account, status);
            let _ = server.kill();
            let _ = server.wait();
            return;
        }
    };
    if let Err(error) = pad_broker::reply(&stream, Status::Served, port, Some(relay.as_fd())) {
        tracing::warn!(error = %format!("{error:#}"), account, port, "seat USB device not handed over");
    }
    drop((stream, relay));
    tracing::info!(account, port, pad = label, "seat USB device served");
    let recorded = client.as_raw_fd();
    pad_broker::keep_usb(port, recorded, client.as_fd());
    watch_usb(port, recorded, OwnedFd::from(client));
    // The pad server ends with the connection; reap it.
    let _ = server.wait();
    tracing::info!(account, port, "seat USB device gone");
}

/// Watches a seat's USB socket until its connection ends, held for its port's seat
/// ([`revoke`]). A socket no seat's row names, or one that can't be held, is shut down at once.
fn watch_usb(port: u16, recorded: RawFd, sock: OwnedFd) {
    let held = vhci::account_at(port)
        .ok_or_else(|| std::io::Error::other("no seat's row names the port"))
        .and_then(|account| Hold::new(&account, sock.as_fd()));
    if let Err(error) = &held {
        tracing::warn!(%error, port, "seat USB device not revocable, detached");
        let _ = rustix::net::shutdown(&sock, Shutdown::Both);
    }
    vhci::watch(port, recorded, sock);
}

/// Starts `hello`'s pad server in a transient scope, so a supervisor restart leaves it running:
/// `(the socket vhci_hcd takes, the relay's seat end, the server)`. The server reads `hello`
/// first off its relay end, which the seat never sees.
fn start_pad_server(account: &str, hello: &Hello) -> anyhow::Result<(TcpStream, OwnedFd, Child)> {
    let (client, served) = usbip::loopback_pair()?;
    let (theirs, seat) = pad_broker::relay_pair()?;
    let seat = UnixDatagram::from(seat);
    seat.send(&hello.encode())
        .context("hand the pad server its hello")?;
    let seat = OwnedFd::from(seat);
    let port = served.local_addr().context("pad server address")?.port();
    let exe = std::env::current_exe().context("find this binary")?;
    let server = Command::new("systemd-run")
        .args(["--scope", "--quiet", "--collect"])
        .arg(format!("--unit=punktfunk-seat-usb-{account}-{port}"))
        .arg("--")
        .arg(exe)
        .arg("serve-usb")
        .stdin(Stdio::from(OwnedFd::from(served)))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .spawn()
        .context("start systemd-run")?;
    Ok((client, seat, server))
}

/// `serve-usb`: a seat's pad server, as [`start_pad_server`] starts it. Stdin is the socket
/// `vhci_hcd` talks to, stdout the pad's relay; it serves until either hangs up.
pub fn serve_usb_device() -> Result<(), String> {
    let take = |fd: std::os::fd::BorrowedFd<'_>, what: &str| {
        fd.try_clone_to_owned()
            .map_err(|e| format!("take the {what}: {e}"))
    };
    let sock = take(std::io::stdin().as_fd(), "host's socket")?;
    let relay = take(std::io::stdout().as_fd(), "pad's relay")?;
    usbip::serve(TcpStream::from(sock), UnixDatagram::from(relay)).map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seat_holds_at_most_the_cap_and_gets_slots_back() {
        let mut held = HashMap::new();
        for _ in 0..MAX_PER_SEAT {
            assert!(admit(&mut held, "pf-seat-1", MAX_PER_SEAT));
        }
        assert!(!admit(&mut held, "pf-seat-1", MAX_PER_SEAT), "one too many");
        assert!(
            admit(&mut held, "pf-seat-2", MAX_PER_SEAT),
            "another seat has its own cap"
        );
        release(&mut held, "pf-seat-1");
        assert!(admit(&mut held, "pf-seat-1", MAX_PER_SEAT));
        for _ in 0..=MAX_PER_SEAT {
            release(&mut held, "pf-seat-1");
        }
        assert!(
            !held.contains_key("pf-seat-1"),
            "a seat with no pads is forgotten"
        );
    }

    /// Unread connections: four per uid, sixteen in all, and a slot back once one is read.
    #[test]
    fn unread_connections_are_capped_per_uid_and_in_all() {
        let mut pending = HashMap::new();
        for _ in 0..PENDING_PER_UID {
            assert!(admit_pending(&mut pending, "1000"));
        }
        assert!(!admit_pending(&mut pending, "1000"), "one uid's cap");
        for uid in 1..PENDING_TOTAL / PENDING_PER_UID {
            for _ in 0..PENDING_PER_UID {
                assert!(admit_pending(&mut pending, &format!("200{uid}")));
            }
        }
        assert!(!admit_pending(&mut pending, "3000"), "the total");
        release(&mut pending, "1000");
        assert!(admit_pending(&mut pending, "3000"));
    }

    /// A stop shuts down that seat's relays and USB sockets and no other seat's: the relay and
    /// the seat read a hang-up, and so does the pad server. A dropped hold leaves the list.
    #[test]
    fn a_stop_takes_back_only_that_seats_sockets() {
        use std::io::{ErrorKind, Read as _};
        let (relay, seat) = pad_broker::relay_pair().unwrap();
        let (usb, mut server) = usbip::loopback_pair().unwrap();
        let (other, _other_seat) = pad_broker::relay_pair().unwrap();
        let held = [
            Hold::new("pf-test-stop", relay.as_fd()).unwrap(),
            Hold::new("pf-test-stop", usb.as_fd()).unwrap(),
            Hold::new("pf-test-other", other.as_fd()).unwrap(),
        ];
        revoke("pf-test-stop");
        assert_eq!(relay.recv(&mut [0; 8]).unwrap(), 0, "the relay");
        assert_eq!(
            UnixDatagram::from(seat).recv(&mut [0; 8]).unwrap(),
            0,
            "the seat"
        );
        assert_eq!(server.read(&mut [0; 8]).unwrap(), 0, "the pad server");
        other.set_nonblocking(true).unwrap();
        let kept = other.recv(&mut [0; 8]).unwrap_err().kind();
        assert_eq!(kept, ErrorKind::WouldBlock, "another seat keeps its pad");
        drop(held);
        let left = holds()
            .values()
            .filter(|(a, _)| a.starts_with("pf-test-"))
            .count();
        assert_eq!(left, 0);
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
