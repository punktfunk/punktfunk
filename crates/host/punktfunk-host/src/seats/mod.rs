//! The box's seats as its own host sees them: the supervisor's ledger over its pipe, who plays
//! on each seat host, and [`placement`], which host a profile's connect goes to.
//!
//! Windows runs the supervisor in the service (`windows/service/seats.rs`); Linux runs it as the
//! root `punktfunk-seats` daemon behind a Unix socket, which only the door (`serve --door`) can
//! reach: a user's own host never lists seats. Elsewhere there is none, every call answers so,
//! and no connect is placed.

pub(crate) mod events;
#[cfg(any(windows, target_os = "linux"))]
pub(crate) mod lifecycle;
pub(crate) mod placement;
#[cfg(target_os = "linux")]
pub(crate) mod session_switch;

use crate::profiles::OsAccount;
use pf_seats::ipc::{ApiError, Command, CommandResult, ErrorCode};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One request to the seat supervisor. Blocking: call it off the async workers.
pub(crate) fn call(command: Command) -> Result<CommandResult, ApiError> {
    #[cfg(windows)]
    {
        pf_seats::windows::pipe::request(pf_seats::ipc::PIPE_NAME, command)
    }
    #[cfg(target_os = "linux")]
    {
        pf_seats::linux::socket::request(
            std::path::Path::new(pf_seats::linux::SOCKET_PATH),
            command,
        )
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = command;
        Err(ApiError::new(
            ErrorCode::Backend,
            "seats run on a Windows Server or Linux host",
        ))
    }
}

/// Whether seats are on: on Windows the operator's marker, on Linux a supervisor that answers.
pub(crate) fn enabled() -> bool {
    #[cfg(windows)]
    {
        pf_seats::windows::seats_enabled()
    }
    #[cfg(target_os = "linux")]
    {
        // The ledger is the cheap question; `Seating` also runs the prerequisite checks.
        list().is_ok()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

fn single_session() -> bool {
    #[cfg(windows)]
    {
        pf_seats::windows::server_edition() == Some(false) && !pf_seats::windows::session_wrapper()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The ledger: every seat, its ports and whether its host runs.
pub(crate) fn list() -> Result<Vec<pf_seats::Seat>, ApiError> {
    match call(Command::List)? {
        CommandResult::List { seats } => Ok(seats),
        other => Err(ApiError::new(
            ErrorCode::Transport,
            format!("seats list answered {other:?}"),
        )),
    }
}

/// The ledger and who plays on each running seat. A profile list reads every seat at once, and
/// reads within [`FRESH`] of the last one share it.
#[derive(Default)]
pub(crate) struct Snapshot {
    /// The operator turned seats on. Linux seats are on whenever the supervisor answers.
    pub on: bool,
    /// A desktop edition of Windows with no Remote Desktop wrapper: one session, so no seat.
    pub single_session: bool,
    pub seats: Vec<pf_seats::Seat>,
    /// Seat id → who streams there, for each running seat whose host answered.
    pub occupants: BTreeMap<String, Vec<Occupant>>,
    /// Seat id → its host's `/status` as it answered, for the same seats.
    pub statuses: BTreeMap<String, serde_json::Value>,
}

impl Snapshot {
    /// A seat's row and its number, counted from 1 in ledger order.
    pub(crate) fn seat(&self, id: &str) -> Option<(&pf_seats::Seat, u8)> {
        let at = self.seats.iter().position(|s| s.id.as_str() == id)?;
        Some((&self.seats[at], at as u8 + 1))
    }

    /// The box owner's row, on a door. A Linux supervisor that didn't answer lists no rows at
    /// all, so its silence is [`Silent`], never `None`.
    pub(crate) fn owner(&self) -> Result<Option<&pf_seats::Seat>, Silent> {
        if !(cfg!(windows) || self.on) {
            return Err(Silent);
        }
        Ok(self.seats.iter().find(|s| s.owner))
    }

    /// The ledger row a profile plays on. A full seat names its own. On a door the owner and a
    /// profile that shares the owner's desktop play in the owner's host, so on the owner's row.
    /// Anywhere else the rest play on the host that was asked.
    pub(crate) fn row_of<'a>(&'a self, account: &'a OsAccount) -> Option<&'a str> {
        row_in(
            account,
            self.owner().ok().flatten(),
            pf_paths::seat::role().is_door(),
        )
    }
}

/// The profile that plays on `seat`: its own, or on the owner's row the first that shares the
/// owner's desktop. The console reaches the seat's host through that profile's proxy.
pub(crate) fn profile_on(
    seat: &pf_seats::Seat,
    profiles: &[crate::profiles::Profile],
) -> Option<String> {
    profiles
        .iter()
        .find(|p| match &p.os_account {
            OsAccount::Seat { seat: Some(id) } => id == seat.id.as_str(),
            OsAccount::Operator => seat.owner,
            _ => false,
        })
        .map(|p| p.id.to_string())
}

/// The seat supervisor didn't list the seats: which rows exist is unknown.
#[derive(Debug)]
pub(crate) struct Silent;

/// [`Snapshot::row_of`] against a ledger's owner row and whether this is a door.
pub(crate) fn row_in<'a>(
    account: &'a OsAccount,
    owner: Option<&'a pf_seats::Seat>,
    door: bool,
) -> Option<&'a str> {
    match account {
        OsAccount::Seat { seat: Some(id) } => Some(id),
        OsAccount::Operator if door => owner.map(|s| s.id.as_str()),
        _ => None,
    }
}

/// Whether the seat supervisor is this host's to ask: Windows, or a Linux door.
pub(crate) fn has_supervisor() -> bool {
    cfg!(windows) || pf_paths::seat::role().is_door()
}

/// How long a snapshot answers for the box: a picker polls every 2 s.
const FRESH: Duration = Duration::from_secs(2);

struct Cache {
    /// Bumped by [`invalidate`]: a read that began before it is not kept.
    epoch: u64,
    last: Option<(Instant, Arc<Snapshot>)>,
}

static LAST: Mutex<Cache> = Mutex::new(Cache {
    epoch: 0,
    last: None,
});

/// Held while one caller reads the seats afresh.
static READING: Mutex<()> = Mutex::new(());

/// The ledger and whether seats are on, with no seat host asked who plays there.
pub(crate) fn ledger() -> Snapshot {
    if !has_supervisor() {
        return Snapshot::default();
    }
    let listed = list();
    // Linux seats are on whenever the supervisor answers.
    let on = if cfg!(windows) {
        enabled()
    } else {
        listed.is_ok()
    };
    let seats = listed.unwrap_or_else(|e| {
        tracing::debug!(code = ?e.code, "seats ledger did not load: {}", e.message);
        Vec::new()
    });
    Snapshot {
        on,
        single_session: single_session(),
        seats,
        occupants: BTreeMap::new(),
        statuses: BTreeMap::new(),
    }
}

/// The seats as they are now, at most [`FRESH`] old. Blocking: call it off the async workers.
pub(crate) fn snapshot() -> Arc<Snapshot> {
    if !has_supervisor() {
        return Arc::default();
    }
    shared(&LAST, &READING, || {
        let mut snap = ledger();
        snap.statuses = ask_hosts(&snap.seats)
            .into_iter()
            .filter_map(|(id, status)| Some((id, status?)))
            .collect();
        snap.occupants = snap
            .statuses
            .iter()
            .map(|(id, status)| (id.clone(), occupants_in(status)))
            .collect();
        snap
    })
}

/// `cache` while it is fresh, else one `read` that every stale caller waiting meanwhile shares.
/// A page's burst of requests asks the supervisor once, not once each over its connection limit.
/// A read that began before an [`invalidate`] is answered but not kept.
fn shared(
    cache: &Mutex<Cache>,
    reading: &Mutex<()>,
    read: impl FnOnce() -> Snapshot,
) -> Arc<Snapshot> {
    let fresh = || {
        let cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = cache.last.as_ref().filter(|(at, _)| at.elapsed() < FRESH);
        (fresh.map(|(_, snap)| Arc::clone(snap)), cache.epoch)
    };
    if let (Some(snap), _) = fresh() {
        return snap;
    }
    let _reading = reading.lock().unwrap_or_else(|e| e.into_inner());
    let (cached, epoch) = fresh();
    if let Some(snap) = cached {
        return snap;
    }
    let snap = Arc::new(read());
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if cache.epoch == epoch {
        cache.last = Some((Instant::now(), Arc::clone(&snap)));
    }
    snap
}

/// The next [`snapshot`] reads afresh: a seat was just started or stopped.
pub(crate) fn invalidate() {
    let mut cache = LAST.lock().unwrap_or_else(|e| e.into_inner());
    cache.epoch += 1;
    cache.last = None;
}

/// Each running seat's `/status`, `None` where its host didn't answer. The hosts are asked at
/// once: a hung one costs one timeout, not one per seat.
fn ask_hosts(seats: &[pf_seats::Seat]) -> BTreeMap<String, Option<serde_json::Value>> {
    std::thread::scope(|scope| {
        let asked: Vec<_> = seats
            .iter()
            .filter(|s| s.runtime.state == pf_seats::RuntimeState::Running)
            .map(|s| (s.id.as_str().to_string(), scope.spawn(move || status_of(s))))
            .collect();
        asked
            .into_iter()
            .map(|(id, asked)| (id, asked.join().ok().flatten()))
            .collect()
    })
}

/// [`ask_hosts`] read as who plays where.
fn ask_who(seats: &[pf_seats::Seat]) -> BTreeMap<String, Option<Vec<Occupant>>> {
    ask_hosts(seats)
        .into_iter()
        .map(|(id, status)| (id, status.as_ref().map(occupants_in)))
        .collect()
}

/// Whether this host has seats to ask: a Linux door, or Windows with seats turned on.
fn in_play() -> bool {
    pf_paths::seat::role().is_door() || (cfg!(windows) && enabled())
}

/// Whether any device `holds` plays on a seat; `None` when a seat host or the ledger can't say.
/// Blocking, as [`snapshot`].
fn any_on_a_seat(holds: impl Fn(&Occupant) -> bool) -> Option<bool> {
    if !in_play() {
        return Some(false);
    }
    seen_on(&ask_who(&list().ok()?), holds)
}

/// A device seen on an answering seat is a yes. Otherwise a seat that didn't answer leaves it
/// open: it may be playing there.
fn seen_on(
    heard: &BTreeMap<String, Option<Vec<Occupant>>>,
    holds: impl Fn(&Occupant) -> bool,
) -> Option<bool> {
    if heard.values().flatten().flatten().any(holds) {
        Some(true)
    } else if heard.values().any(Option::is_none) {
        None
    } else {
        Some(false)
    }
}

/// Whether a device other than `fp_hex` streams on any seat: the host-power busy check for a
/// box whose sessions run on seat hosts. A guess would cut a live stream, so a seat that can't
/// say counts as a yes. Blocking, as [`snapshot`].
pub(crate) fn other_client_on_a_seat(fp_hex: &str) -> bool {
    any_on_a_seat(|o| !crate::session_status::is_client(&o.client, fp_hex)).unwrap_or(true)
}

/// Whether `fp_hex` streams on a seat; `None` when a seat can't say. Blocking, as [`snapshot`].
pub(crate) fn plays_on_a_seat(fp_hex: &str) -> Option<bool> {
    any_on_a_seat(|o| crate::session_status::is_client(&o.client, fp_hex))
}

/// [`plays_on_a_seat`], `None` once `deadline` has passed. A supervisor that accepts and never
/// answers would hold a caller for minutes, and the startup sweep is one.
pub(crate) fn plays_on_a_seat_by(fp_hex: &str, deadline: Instant) -> Option<bool> {
    if !in_play() {
        return Some(false);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let fp = fp_hex.to_owned();
    std::thread::spawn(move || {
        let _ = tx.send(plays_on_a_seat(&fp));
    });
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .flatten()
}

/// Ends every session on every running seat, ahead of a power action that takes the box down:
/// a seat host doesn't hear the door's own. Blocking, as [`snapshot`].
pub(crate) fn end_all_sessions() {
    if !in_play() {
        return;
    }
    let Ok(seats) = list() else { return };
    std::thread::scope(|scope| {
        for seat in &seats {
            if seat.runtime.state == pf_seats::RuntimeState::Running {
                scope.spawn(move || {
                    if !end_sessions_for_power(seat) {
                        tracing::warn!(seat = %seat.name, "seat sessions did not end before a power action");
                    }
                });
            }
        }
    });
}

/// A seat host's own config dir, where it keeps its management token.
fn host_dir(seat: &pf_seats::Seat) -> PathBuf {
    pf_seats::persistence::default_root()
        .join("hosts")
        .join(seat.id.as_str())
}

/// The management token `seat`'s host was started with, as the supervisor left it for the door.
#[cfg(target_os = "linux")]
pub(crate) fn token_of(seat: &pf_seats::Seat) -> Option<String> {
    crate::mgmt_token::read_persisted(&host_dir(seat))
}

/// A seat host whose binary an update replaced restarts once nobody plays on it: its runner
/// starts the new one. The door and a user host are restarted by their own units.
#[cfg(target_os = "linux")]
pub(crate) fn restart_when_replaced() {
    let spawned = std::thread::Builder::new()
        .name("seat-exe-watch".into())
        .spawn(|| loop {
            std::thread::sleep(Duration::from_secs(60));
            let idle = crate::session_status::snapshot().is_empty()
                && crate::session_status::games().is_empty();
            if pf_paths::exe_replaced() && idle {
                tracing::info!("host binary replaced by an update: restarting while idle");
                let _ = crate::native::stop_self();
                return;
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "seat binary watch did not start");
    }
}

/// The seat host's certificate pin from the ledger, `None` until the supervisor learned it.
fn seat_pin(seat: &pf_seats::Seat) -> Option<[u8; 32]> {
    punktfunk_core::fp::parse_hex32(seat.fingerprint.as_deref()?)
}

/// A device streaming on a seat host, as its `/status` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Occupant {
    /// Fingerprint prefix, or the peer address of an anonymous client.
    pub client: String,
    pub name: Option<String>,
}

/// Loopback is local: a seat that does not answer in this long is not serving.
const STATUS_TIMEOUT: Duration = Duration::from_secs(1);

/// `seat`'s host's loopback `/status`. `None` when the seat host does not answer.
fn status_of(seat: &pf_seats::Seat) -> Option<serde_json::Value> {
    let client = crate::ctl::client::Client::seat(
        seat_pin(seat)?,
        &host_dir(seat),
        seat.mgmt_port,
        Some(STATUS_TIMEOUT),
    )
    .ok()?;
    client.get("/api/v1/status").ok()
}

/// Who streams on a seat, as its `/status` lists them.
pub(crate) fn occupants_in(status: &serde_json::Value) -> Vec<Occupant> {
    status
        .get("sessions")
        .and_then(|rows| rows.as_array())
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some(Occupant {
                client: row.get("client")?.as_str()?.to_string(),
                name: row
                    .get("client_name")
                    .and_then(|n| n.as_str())
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Who streams on `seat` right now. `None` when the seat host does not answer.
pub(crate) fn occupants(seat: &pf_seats::Seat) -> Option<Vec<Occupant>> {
    status_of(seat).as_ref().map(occupants_in)
}

/// Ends every session on `seat` through its host's loopback API. `false` when it didn't answer.
pub(crate) fn end_sessions(seat: &pf_seats::Seat) -> bool {
    stop_on(seat, "/api/v1/session")
}

/// [`end_sessions`] for a power action: the seat host closes each session as the door's own
/// close, so its clients read that the box is going down. A seat host that predates the
/// reason ends them plainly.
fn end_sessions_for_power(seat: &pf_seats::Seat) -> bool {
    stop_on(seat, "/api/v1/session?reason=power")
}

fn stop_on(seat: &pf_seats::Seat, path: &str) -> bool {
    let Some(pin) = seat_pin(seat) else {
        return false;
    };
    crate::ctl::client::Client::seat(pin, &host_dir(seat), seat.mgmt_port, Some(STATUS_TIMEOUT))
        .and_then(|client| client.delete(path))
        .is_ok()
}

/// `call` on `seat`'s host's API, for the door and the console's proxy. `None` when the seat
/// host doesn't answer. A tail has no deadline: it ends when the caller lets go.
pub(crate) fn forward(
    seat: &pf_seats::Seat,
    call: crate::mgmt::owner::Forwarded,
) -> Option<crate::mgmt::owner::Answer> {
    forward_within(seat, call, PROXY_TIMEOUT)
}

/// [`forward`], answered within `timeout` rather than [`PROXY_TIMEOUT`].
pub(crate) fn forward_within(
    seat: &pf_seats::Seat,
    call: crate::mgmt::owner::Forwarded,
    timeout: Duration,
) -> Option<crate::mgmt::owner::Answer> {
    use crate::mgmt::owner::Answer;
    let timeout = (!call.stream).then_some(timeout);
    let client =
        crate::ctl::client::Client::seat(seat_pin(seat)?, &host_dir(seat), seat.mgmt_port, timeout)
            .ok()?;
    let path = format!("/api/v1/{}", call.target);
    let sent = client.send(
        &call.method,
        &path,
        call.content_type.as_deref(),
        call.device.as_deref(),
        call.body,
    );
    let (status, content_type, body) = sent.ok()?;
    if call.stream {
        return Some(Answer::Stream(status, content_type, tail_body(body)));
    }
    let bytes = crate::ctl::client::read_capped(body, crate::ctl::client::RAW_BODY_LIMIT).ok()?;
    Some(Answer::Reply(status, content_type, bytes))
}

/// `path` (under `/api/v1/`) on `seat`'s host as it arrives, for as long as both sides keep
/// it. `None` when the seat host doesn't answer, or answers with anything but a `200`.
pub(crate) fn tail(seat: &pf_seats::Seat, path: &str) -> Option<Box<dyn std::io::Read + Send>> {
    let client =
        crate::ctl::client::Client::seat(seat_pin(seat)?, &host_dir(seat), seat.mgmt_port, None)
            .ok()?;
    let (status, _, body) = client
        .send("GET", &format!("/api/v1/{path}"), None, None, Vec::new())
        .ok()?;
    (status == 200).then_some(body)
}

/// A proxied library page with art can take a while on a cold seat.
const PROXY_TIMEOUT: Duration = Duration::from_secs(30);

/// `body` as it arrives, on a thread of its own: a reader that blocks must not hold a worker.
/// Dropping the body ends the read, and with it the connection.
fn tail_body(mut body: Box<dyn std::io::Read + Send>) -> axum::body::Body {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(8);
    std::thread::Builder::new()
        .name("seat-tail".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                let sent = match body.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => tx.blocking_send(Ok(axum::body::Bytes::copy_from_slice(&buf[..n]))),
                    Err(e) => {
                        let _ = tx.blocking_send(Err(e));
                        break;
                    }
                };
                if sent.is_err() {
                    break;
                }
            }
        })
        .ok();
    axum::body::Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|chunk| (chunk, rx))
    }))
}

#[cfg(test)]
mod tests {
    use super::{seen_on, Occupant};
    use std::collections::BTreeMap;

    /// Callers that find the snapshot stale at once share one read of the seats.
    #[test]
    fn a_burst_of_stale_readers_reads_the_seats_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;
        let cache = Mutex::new(super::Cache {
            epoch: 0,
            last: None,
        });
        let (reading, reads) = (Mutex::new(()), AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    super::shared(&cache, &reading, || {
                        reads.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        super::Snapshot::default()
                    })
                });
            }
        });
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn only_the_callers_own_seat_sessions_leave_the_box_free() {
        let fp = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let on = |client: &str| {
            Some(vec![Occupant {
                client: client.into(),
                name: None,
            }])
        };
        let other = |seats: &BTreeMap<String, Option<Vec<Occupant>>>| {
            seen_on(seats, |o| !crate::session_status::is_client(&o.client, fp)).unwrap_or(true)
        };
        let mut seats = BTreeMap::new();
        seats.insert("owner".to_string(), on(&fp[..12]));
        seats.insert("seat-1".to_string(), Some(vec![]));
        assert!(!other(&seats), "the caller's own stream");
        seats.insert("seat-2".to_string(), on("112233445566"));
        assert!(other(&seats), "another device on a seat");
        seats.insert("seat-2".to_string(), on("192.168.1.50"));
        assert!(other(&seats), "an anonymous client is always someone else");
        seats.insert("seat-2".to_string(), None);
        assert!(other(&seats), "a seat host that doesn't answer is busy");
    }

    /// A guest seen on one seat is there whatever the others say; one nobody saw is unknown
    /// while a seat is silent, and gone once every seat has answered.
    #[test]
    fn a_silent_seat_leaves_a_guest_open_unless_it_was_seen() {
        let fp = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let mine = |o: &Occupant| crate::session_status::is_client(&o.client, fp);
        let on = Some(vec![Occupant {
            client: fp[..12].into(),
            name: None,
        }]);
        let mut seats = BTreeMap::new();
        seats.insert("seat-1".to_string(), Some(vec![]));
        assert_eq!(seen_on(&seats, mine), Some(false), "every seat answered");
        seats.insert("seat-2".to_string(), None);
        assert_eq!(seen_on(&seats, mine), None, "seat-2 may have it");
        seats.insert("seat-1".to_string(), on);
        assert_eq!(seen_on(&seats, mine), Some(true), "seen on seat-1");
    }
}
