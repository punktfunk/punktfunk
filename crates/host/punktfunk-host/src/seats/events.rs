//! The door's events and the seats': one bus for the console. The seats' hosts stream, so
//! their sessions, games, library and plugin changes happen there; the door pairs and places,
//! so knocks, grants and power happen here. Each side's events reach the other's bus: the
//! seats' into the door's, for the console; the door's into the owner's host, whose hooks run
//! in the owner's session. The two kind sets are disjoint, so nothing comes back around.
//!
//! A seat's `game.*` also reaches the host with the plugins: the owner's on Linux, the box host
//! itself on Windows, which tails its seats' games alone. A held launch is released on the seat
//! once those plugins are done. An event that already names a seat is one a host relayed and is
//! never taken back.

use crate::events::{EventKind, HostEvent};
use std::collections::HashMap;
use std::io::{BufRead, Read};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Event kinds the door hands to the owner's host: what happens on the door alone.
const RELAYED: &[&str] = &["pairing.*", "access.*", "host.*", "action.*", "update.*"];

/// Whether `kind` is the door's own, handed to the owner's host.
fn relayed(kind: &str) -> bool {
    RELAYED
        .iter()
        .any(|pattern| crate::events::kind_matches(pattern, kind))
}

/// How often the door looks for a running seat it has no tail on.
const TICK: Duration = Duration::from_secs(3);

/// Tails every running seat's events into this host's bus, for the host's lifetime. `kinds` is
/// the stream's `&kinds=` filter, empty for all of them. Windows asks the service only while
/// seats are on.
pub(crate) fn fan_in(kinds: &'static str) {
    std::thread::Builder::new()
        .name("seat-events".into())
        .spawn(move || {
            let cursors: Arc<Mutex<HashMap<String, u64>>> = Arc::default();
            let mut tails: HashMap<String, std::thread::JoinHandle<()>> = HashMap::new();
            loop {
                tails.retain(|_, tail| !tail.is_finished());
                if cfg!(windows) && !super::enabled() {
                    std::thread::sleep(TICK);
                    continue;
                }
                // The ledger alone: asking each seat host would poll them all every tick.
                let ledger = super::ledger();
                for seat in &ledger.seats {
                    let id = seat.id.as_str().to_string();
                    if seat.runtime.state != pf_seats::RuntimeState::Running {
                        // All it says from its next start is new. A launch held there waits on
                        // this tail, which can connect ticks after the client does.
                        let mut cursors = cursors.lock().unwrap_or_else(|e| e.into_inner());
                        cursors.insert(id, 0);
                        continue;
                    }
                    if tails.contains_key(&id) {
                        continue;
                    }
                    let Some(profile) = profile_of(seat) else {
                        continue;
                    };
                    let seat = seat.clone();
                    let cursors = Arc::clone(&cursors);
                    let tail = std::thread::Builder::new()
                        .name(format!("seat-events-{id}"))
                        .spawn(move || tail(&seat, profile, &cursors, kinds));
                    if let Ok(tail) = tail {
                        tails.insert(id, tail);
                    }
                }
                std::thread::sleep(TICK);
            }
        })
        .ok();
}

/// The profile whose host `seat` runs: its own, or the owner's for the owner's row.
fn profile_of(seat: &pf_seats::Seat) -> Option<String> {
    super::profile_on(
        seat,
        &crate::profiles::Profiles::load_with(None, None).list(),
    )
}

/// Reads `seat`'s events from where this host last saw them, into this host's bus as
/// `profile`'s, until the connection ends. The next tick starts it over. A seat's games also
/// go to [`hand_on`], in order, on a thread of their own.
fn tail(
    seat: &pf_seats::Seat,
    profile: String,
    cursors: &Mutex<HashMap<String, u64>>,
    kinds: &str,
) {
    let id = seat.id.as_str().to_string();
    // A seat already running when this host came up: only what happens from now on, never a
    // replay.
    let since = cursors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .copied()
        .unwrap_or(u64::MAX);
    let Some(body) = super::tail(seat, &format!("events?since={since}{kinds}")) else {
        return;
    };
    let games = (!seat.owner).then(|| games_of(seat.clone())).flatten();
    for (event, data) in frames(std::io::BufReader::new(body)) {
        if event == "dropped" || event == "live" {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<HostEvent>(&data) else {
            continue;
        };
        cursors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), ev.seq);
        let what = take(&ev);
        if what == Take::Skip {
            continue;
        }
        let ev = crate::events::bus().emit_from(ev.kind, Some(profile.clone()));
        if let (Take::HandOn, Some(games)) = (what, &games)
            && games.try_send(ev).is_err()
        {
            tracing::debug!(seat = %seat.name, "seat's game event not handed on: queue full");
        }
    }
}

/// What a host does with one event off a seat's stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Take {
    Skip,
    Emit,
    /// Emit, and hand it to the host with the plugins.
    HandOn,
}

/// One that names a seat already was relayed there, and the door's own kinds start here: both
/// skipped. A game goes on to the plugins.
fn take(ev: &HostEvent) -> Take {
    let kind = ev.kind.name();
    if ev.seat.is_some() || relayed(kind) {
        Take::Skip
    } else if crate::events::kind_matches("game.*", kind) {
        Take::HandOn
    } else {
        Take::Emit
    }
}

/// How many of a seat's game events wait for [`hand_on`]. One launch is a handful; past this a
/// seat is flooding, and the rest are dropped.
const GAMES_QUEUE: usize = 32;

/// The thread that hands `seat`'s games on, in order. It ends with the sender.
fn games_of(seat: pf_seats::Seat) -> Option<std::sync::mpsc::SyncSender<HostEvent>> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<HostEvent>(GAMES_QUEUE);
    std::thread::Builder::new()
        .name(format!("seat-games-{}", seat.id.as_str()))
        .spawn(move || {
            for ev in rx {
                hand_on(&seat, &ev);
            }
        })
        .ok()?;
    Some(tx)
}

/// One of `seat`'s games to the host with the plugins: this one on Windows, which only runs a
/// held launch's plugins, else the owner's. A held launch is then released on the seat, whatever
/// the plugins answered; when nothing answers, the seat's own deadline ends the wait.
fn hand_on(seat: &pf_seats::Seat, ev: &HostEvent) {
    let hold = match &ev.kind {
        EventKind::GameLaunching { hold, .. } => hold.as_deref().filter(|t| is_token(t)),
        _ => None,
    };
    if cfg!(windows) {
        if hold.is_some() {
            crate::holds::plugins(ev);
        }
    } else {
        to_owner(ev, hold.is_some());
    }
    let Some(token) = hold else {
        return;
    };
    let call = crate::mgmt::owner::Forwarded {
        method: "POST".into(),
        target: format!("game/holds/{token}/release"),
        content_type: None,
        body: Vec::new(),
        device: None,
        stream: false,
        start: false,
    };
    if super::forward(seat, call).is_none() {
        tracing::debug!(seat = %seat.name, "seat's launch hold not released");
    }
}

/// A hold token as a seat host makes it: hex, short enough for a path segment.
fn is_token(token: &str) -> bool {
    (1..=128).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A seat's game to the owner's host, when it runs. `held` waits out its plugins' hold.
fn to_owner(ev: &HostEvent, held: bool) {
    let ledger = super::ledger();
    let Ok(Some(owner)) = ledger.owner() else {
        return;
    };
    if owner.runtime.state != pf_seats::RuntimeState::Running {
        return;
    }
    let timeout = if held {
        crate::holds::RELAYED_HOLD
    } else {
        super::PROXY_TIMEOUT
    };
    if !post_relay(owner, ev, timeout) {
        tracing::debug!(
            kind = ev.kind.name(),
            "seat's game not relayed to the owner's host"
        );
    }
}

/// `ev` to the owner's host's `POST /events/relay`, answered within `timeout`. `false` when it
/// wasn't.
fn post_relay(owner: &pf_seats::Seat, ev: &HostEvent, timeout: Duration) -> bool {
    let Ok(body) = serde_json::to_vec(&serde_json::json!({ "event": ev })) else {
        return false;
    };
    let call = crate::mgmt::owner::Forwarded {
        method: "POST".into(),
        target: "events/relay".into(),
        content_type: Some("application/json".into()),
        body,
        device: None,
        stream: false,
        start: false,
    };
    super::forward_within(owner, call, timeout).is_some()
}

/// The most a seat's stream may send between two blank lines. A seat host runs as a guest, and
/// the door's memory serves the whole box: past this the tail ends.
const FRAME_LIMIT: u64 = 1 << 20;

/// The `(event, data)` of each SSE frame in `reader`, in order. Ends with the stream, or at a
/// frame longer than [`FRAME_LIMIT`].
fn frames(mut reader: impl std::io::BufRead) -> impl Iterator<Item = (String, String)> {
    std::iter::from_fn(move || {
        let (mut event, mut data) = (String::new(), Vec::new());
        let (mut raw, mut left) = (Vec::new(), FRAME_LIMIT);
        loop {
            raw.clear();
            let n = (&mut reader).take(left).read_until(b'\n', &mut raw).ok()?;
            if raw.last() != Some(&b'\n') {
                return None;
            }
            left -= n as u64;
            let line = std::str::from_utf8(&raw).ok()?;
            let line = line.strip_suffix('\n').unwrap_or(line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                if event.is_empty() && data.is_empty() {
                    left = FRAME_LIMIT;
                    continue;
                }
                return Some((event, data.join("\n")));
            }
            if let Some(value) = line.strip_prefix("event:") {
                event = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data:") {
                data.push(value.strip_prefix(' ').unwrap_or(value).to_string());
            }
        }
    })
}

/// The one relayed kind that wakes a stopped owner's session: someone is waiting on an answer.
const WAKES: &str = "pairing.pending";

/// How long a knock waits for the owner's host to come up before its hooks are skipped.
const WAKE_WAIT: Duration = Duration::from_secs(120);

/// Hands each of this door's own events to the owner's host, for the owner's hooks. A knock
/// wakes a stopped owner's session and waits for its host, and for a supervisor that isn't
/// answering; any other event a stopped or silent host misses.
pub(crate) async fn relay_to_owner() {
    let mut rx = crate::events::bus().subscribe_live();
    loop {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        if ev.seat.is_some() || !relayed(ev.kind.name()) {
            continue;
        }
        let wakes = ev.kind.name() == WAKES;
        tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + WAKE_WAIT;
            let mut asked = false;
            loop {
                let ledger = super::ledger();
                let waiting = wakes && std::time::Instant::now() < deadline;
                let owner = match ledger.owner() {
                    Ok(Some(owner)) => owner,
                    Err(_) if waiting => {
                        std::thread::sleep(Duration::from_secs(5));
                        continue;
                    }
                    _ => return,
                };
                if owner.runtime.state == pf_seats::RuntimeState::Running {
                    if !post_relay(owner, &ev, super::PROXY_TIMEOUT) {
                        tracing::debug!(
                            kind = ev.kind.name(),
                            "event not relayed to the owner's host"
                        );
                    }
                    return;
                }
                if !waiting {
                    return;
                }
                if !asked {
                    asked = true;
                    let started = super::call(pf_seats::ipc::Command::Start {
                        id: owner.id.clone(),
                    });
                    if let Err(error) = started {
                        tracing::warn!(code = ?error.code, "owner's session did not start for a knock: {}", error.message);
                        return;
                    }
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_on_the_blank_line_and_join_data_lines() {
        let raw = "event: live\ndata: {\"live\":true}\n\nid: 7\nevent: library.changed\ndata: {\"a\":1,\ndata: \"b\":2}\n\n\n";
        let got: Vec<_> = frames(std::io::Cursor::new(raw)).collect();
        assert_eq!(
            got,
            vec![
                ("live".to_string(), "{\"live\":true}".to_string()),
                (
                    "library.changed".to_string(),
                    "{\"a\":1,\n\"b\":2}".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_frame_past_the_limit_ends_the_stream() {
        let long = "x".repeat(FRAME_LIMIT as usize);
        let raw = format!("event: a\ndata: 1\n\ndata: {long}\n\nevent: b\ndata: 2\n\n");
        let got: Vec<_> = frames(std::io::Cursor::new(raw)).collect();
        assert_eq!(got, vec![("a".to_string(), "1".to_string())]);
        let keep_alives = ":\n\n".repeat(FRAME_LIMIT as usize);
        let raw = format!("{keep_alives}event: c\ndata: 3\n\n");
        assert_eq!(frames(std::io::Cursor::new(raw)).count(), 1);
    }

    /// What the door originates goes to the owner; what a seat originates comes to the door.
    /// A kind in both would come back around for good.
    #[test]
    fn the_two_kind_sets_are_disjoint() {
        for kind in [
            "pairing.pending",
            "access.granted",
            "host.started",
            "update.applied",
        ] {
            assert!(relayed(kind), "{kind}");
        }
        for kind in [
            "session.started",
            "stream.started",
            "game.exited",
            "library.changed",
            "plugins.changed",
            "settings.changed",
            "display.created",
        ] {
            assert!(!relayed(kind), "{kind}");
        }
    }

    /// A seat's games go on to the plugins; anything already tagged with a seat came from a
    /// relay and goes nowhere, so the owner's host's copy never comes back to the door.
    #[test]
    fn only_a_seat_s_own_games_are_handed_on() {
        let ev = |kind: serde_json::Value, seat: Option<&str>| {
            let mut ev: HostEvent = serde_json::from_value(kind).unwrap();
            ev.seat = seat.map(str::to_string);
            ev
        };
        let game = |kind: &str| {
            serde_json::json!({"seq": 1, "ts_ms": 1, "schema": 1, "kind": kind,
                "game": {"title": "Dota 2", "client": "deck", "plane": "native"},
                "reason": "exited", "hold": "ab12"})
        };
        let library = serde_json::json!({"seq": 1, "ts_ms": 1, "schema": 1,
            "kind": "library.changed", "source": "manual"});
        let knock = serde_json::json!({"seq": 1, "ts_ms": 1, "schema": 1,
            "kind": "pairing.pending",
            "device": {"name": "Deck", "fingerprint": "ab", "plane": "native"}});
        assert_eq!(take(&ev(game("game.launching"), None)), Take::HandOn);
        assert_eq!(take(&ev(game("game.exited"), None)), Take::HandOn);
        assert_eq!(take(&ev(library.clone(), None)), Take::Emit);
        assert_eq!(take(&ev(knock, None)), Take::Skip);
        assert_eq!(take(&ev(game("game.exited"), Some("9a3f"))), Take::Skip);
        assert_eq!(take(&ev(library, Some("9a3f"))), Take::Skip);
    }

    #[test]
    fn a_hold_token_is_hex_for_one_path_segment() {
        assert!(is_token(&crate::mgmt_token::random_token()));
        for bad in ["", "../status", "ab/cd", "ab?x=1", &"a".repeat(129)] {
            assert!(!is_token(bad), "{bad}");
        }
    }
}
