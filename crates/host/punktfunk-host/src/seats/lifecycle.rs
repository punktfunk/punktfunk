//! What keeps seats up and what lets them go: the seats of recent players start with the host,
//! and a seat nobody plays on stops after **Stop an idle seat after** (`seat_idle_stop`).
//!
//! The Windows service holds the supervisor in-process; the door on Linux reaches it over its
//! socket. Both run this through [`Supervisor`].

use crate::profiles::Profile;
use pf_seats::ipc::{ApiError, Command};
use pf_seats::{RuntimeState, Seat, SeatId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A profile played within this long gets its seat started at boot.
const WARM_WITHIN_SECS: u64 = 14 * 24 * 3600;
const IDLE_CHECK: Duration = Duration::from_secs(300);

/// How long a seat may sit with nobody on it before it is stopped: it holds a display slot, a
/// GPU context and a session for no one. `None` never stops one.
fn idle_stop() -> Option<Duration> {
    match pf_host_config::config().seat_idle_stop_min {
        0 => None,
        min => Some(Duration::from_secs(u64::from(min) * 60)),
    }
}

/// A seat supervisor as keep-warm and the idle stop see it.
pub(crate) trait Supervisor {
    /// The ledger.
    fn seats(&self) -> Vec<Seat>;
    /// Whether seats are on.
    fn seats_on(&self) -> bool;
    fn start(&self, id: SeatId) -> Result<(), ApiError>;
    fn stop(&self, id: SeatId) -> Result<(), ApiError>;
}

#[cfg(windows)]
impl Supervisor for pf_seats::SeatService<pf_seats::WindowsBackend> {
    fn seats(&self) -> Vec<Seat> {
        self.ledger().seats
    }

    fn seats_on(&self) -> bool {
        pf_seats::windows::seats_enabled()
    }

    fn start(&self, id: SeatId) -> Result<(), ApiError> {
        self.dispatch(Command::Start { id }).map(drop)
    }

    fn stop(&self, id: SeatId) -> Result<(), ApiError> {
        self.dispatch(Command::Stop { id }).map(drop)
    }
}

/// The supervisor's socket, from the door.
#[cfg(target_os = "linux")]
pub(crate) struct Socket;

#[cfg(target_os = "linux")]
impl Supervisor for Socket {
    fn seats(&self) -> Vec<Seat> {
        super::list().unwrap_or_default()
    }

    fn seats_on(&self) -> bool {
        super::enabled()
    }

    fn start(&self, id: SeatId) -> Result<(), ApiError> {
        super::call(Command::Start { id }).map(drop)
    }

    fn stop(&self, id: SeatId) -> Result<(), ApiError> {
        super::call(Command::Stop { id }).map(drop)
    }
}

/// The rows of the `wanted` most recent players, newest first, once each. `now` is unix seconds.
fn warm_rows(
    profiles: &[Profile],
    owner: Option<&Seat>,
    door: bool,
    now: u64,
    wanted: usize,
) -> Vec<String> {
    let mut recent: Vec<(u64, &str)> = profiles
        .iter()
        .filter(|p| p.last_used_unix > 0 && now.saturating_sub(p.last_used_unix) < WARM_WITHIN_SECS)
        .filter_map(|p| Some((p.last_used_unix, super::row_in(&p.os_account, owner, door)?)))
        .collect();
    recent.sort_by_key(|r| std::cmp::Reverse(r.0));
    let mut rows: Vec<String> = Vec::new();
    for (_, row) in recent {
        if !rows.iter().any(|r| r == row) {
            rows.push(row.to_string());
        }
    }
    rows.truncate(wanted);
    rows
}

/// Starts the seats of the most recent players, up to **Seats kept warm**, one at a time: a
/// first logon is heavy, and several at once starve each other.
pub(crate) fn keep_warm(sup: &dyn Supervisor) {
    let wanted = pf_host_config::config().steam_prewarm as usize;
    if wanted == 0 || !sup.seats_on() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let profiles = crate::profiles::Profiles::load_with(None, None).list();
    let ledger = sup.seats();
    let owner = ledger.iter().find(|s| s.owner);
    let door = pf_paths::seat::is_door();
    for row in warm_rows(&profiles, owner, door, now, wanted) {
        let Ok(id) = SeatId::parse(row) else { continue };
        let running = ledger
            .iter()
            .any(|s| s.id == id && s.runtime.state == RuntimeState::Running);
        if running {
            continue;
        }
        if let Err(error) = sup.start(id) {
            tracing::warn!(code = ?error.code, "kept-warm seat did not start: {}", error.message);
        }
    }
}

/// Stops a running seat once nobody has played on it for [`idle_stop`]. A seat whose host
/// doesn't answer counts as busy: stopping it would be a guess.
pub(crate) fn stop_idle(sup: &dyn Supervisor, stop: &AtomicBool) {
    let Some(limit) = idle_stop() else { return };
    let mut busy_at: HashMap<String, Instant> = HashMap::new();
    while !stop.load(Ordering::SeqCst) {
        let mut waited = Duration::ZERO;
        while waited < IDLE_CHECK {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
            waited += Duration::from_secs(1);
        }
        let now = Instant::now();
        for seat in sup.seats() {
            let id = seat.id.as_str().to_string();
            if seat.runtime.state != RuntimeState::Running {
                busy_at.remove(&id);
                continue;
            }
            let busy = super::occupants(&seat).is_none_or(|o| !o.is_empty());
            let since = busy_at.entry(id.clone()).or_insert(now);
            if busy {
                *since = now;
            } else if now.duration_since(*since) >= limit {
                tracing::info!(seat = %id, name = %seat.name, idle_min = limit.as_secs() / 60, "idle seat stopped");
                if let Err(error) = sup.stop(seat.id) {
                    tracing::warn!(code = ?error.code, "idle seat did not stop: {}", error.message);
                }
                busy_at.remove(&id);
            }
        }
    }
}

/// The door's share: waits for the supervisor, warms the seats, then watches for idle ones. It
/// outlives nothing: it ends with the process.
#[cfg(target_os = "linux")]
pub(crate) fn run_door() {
    let spawned = std::thread::Builder::new()
        .name("seats-door".into())
        .spawn(|| {
            // The door starts beside the supervisor, which opens its socket a moment later.
            for _ in 0..60 {
                if Socket.seats_on() {
                    break;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
            keep_warm(&Socket);
            stop_idle(&Socket, &AtomicBool::new(false));
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "door seat upkeep did not start");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{OsAccount, SeatTier};

    fn seat(id: &str, owner: bool) -> Seat {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": "n", "account": "a", "display_slot": 12,
            "native_port": 9778, "mgmt_port": 47995, "owner": owner,
        }))
        .unwrap()
    }

    fn profile(id: &str, account: OsAccount, last_used: u64) -> Profile {
        let mut p: Profile = serde_json::from_value(serde_json::json!({
            "id": id, "display_name": id, "os_account": {"kind": "operator"},
        }))
        .unwrap();
        p.os_account = account;
        p.last_used_unix = last_used;
        p
    }

    const OWNER: &str = "0123456789abcdef0123456789abcdef";
    const KID: &str = "fedcba9876543210fedcba9876543210";

    fn full(id: &str) -> OsAccount {
        OsAccount::Seat {
            seat: Some(id.into()),
            tier: SeatTier::Full,
        }
    }

    /// On a door the owner and a light seat warm the owner's row, a full seat its own; the
    /// newest first, each row once, nothing older than two weeks, and no more than asked.
    #[test]
    fn the_most_recent_players_rows_warm_once_each() {
        let owner = seat(OWNER, true);
        let now = 10_000_000;
        let light = OsAccount::Seat {
            seat: None,
            tier: SeatTier::Light,
        };
        let profiles = [
            profile("owner", OsAccount::Operator, now - 300),
            profile("light", light.clone(), now - 100),
            profile("kid", full(KID), now - 200),
            profile("old", full("ff".repeat(16).as_str()), now - 15 * 24 * 3600),
            profile("never", full(KID), 0),
        ];
        assert_eq!(
            warm_rows(&profiles, Some(&owner), true, now, 8),
            vec![OWNER.to_string(), KID.to_string()]
        );
        assert_eq!(
            warm_rows(&profiles, Some(&owner), true, now, 1),
            vec![OWNER]
        );
        // Off a door only a full seat has a row, and with no owner row there is none to warm.
        assert_eq!(warm_rows(&profiles, None, false, now, 8), vec![KID]);
        assert!(warm_rows(&profiles[..2], None, true, now, 8).is_empty());
    }
}
