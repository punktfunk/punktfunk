//! The pre-warmed seat Steam (`design/steam-seats-warm-launch-implementation-plan.md` WP-S2).
//!
//! Steam's cold boot is 13–30 s of a dedicated launch and nothing on our side shortens it, so the
//! host stands a seat's gamescope and Big Picture up *before* that client asks and parks the
//! display for its fingerprint ([`crate::vdisplay::registry::park`]). The connect then takes the
//! ordinary keep-alive reuse path into a Steam that is already running.
//!
//! [`record`] is written whenever a Steam launch runs under a seat home; [`spawn_run`] reads
//! those records at host start and at session end. A seat is only pre-warmed while its next
//! connect would take the same gamescope spawn route, so the parked display's reuse key is the
//! one that connect asks for. `PUNKTFUNK_STEAM_PREWARM` caps how many seats are held; `0` is off.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{seat_id, session_isolation};
use crate::vdisplay::{Compositor, GamescopeRoute};
use punktfunk_core::Mode;

/// What a seat's Big Picture is parked as. The registry does not key reuse on the launch command
/// — a kept spawn serves any title — so this is the client Steam, not a game.
const PREWARM_LAUNCH: &str = "steam -gamepadui";

/// How long after a seat's last Steam launch it is still worth holding a Steam for. A device
/// that has not played in a fortnight is not the one about to connect.
const WINDOW_SECS: i64 = 14 * 24 * 60 * 60;

/// One run at a time: host start and a session ending land together on a box that restarts a
/// stream, and standing two gamescopes up for one seat breaks its socket lock.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// What pre-warming a seat needs, beside that seat's home.
///
/// Colourimetry and cursor mode are the registry's reuse keys: a display parked without the ones
/// that client asks for is never handed back, and the work is wasted. The mode is the best guess
/// at what it will ask for — on a gamescope that can be resized it is only that.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SeatRecord {
    /// The device's full cert fingerprint, lowercase hex. The file name is only its seat id.
    fingerprint: String,
    width: u32,
    height: u32,
    refresh_hz: u32,
    #[serde(default)]
    hdr: bool,
    #[serde(default)]
    hw_cursor: bool,
    /// Unix seconds of that launch; the window is measured from here.
    last_steam_launch: i64,
}

impl SeatRecord {
    fn mode(&self) -> Mode {
        Mode {
            width: self.width,
            height: self.height,
            refresh_hz: self.refresh_hz,
        }
    }
}

/// Remember what a Steam launch under a seat home streamed at, so the host can bring that seat
/// back up before its next connect. Best-effort: a record that does not land costs a cold Steam.
pub(crate) fn record(fp_hex: &str, mode: Mode, hdr: bool, hw_cursor: bool) {
    let rec = SeatRecord {
        fingerprint: fp_hex.to_string(),
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
        hdr,
        hw_cursor,
        last_steam_launch: crate::clock::unix_secs(),
    };
    if let Err(e) = write(&seat_id(fp_hex), &rec) {
        tracing::warn!(error = %e, "seat record not written — this seat is not pre-warmed");
    }
}

/// Pre-warm on a thread of its own: standing a gamescope up blocks until its first PipeWire
/// node, and `why` is the trigger for the log.
pub(crate) fn spawn_run(why: &'static str) {
    let cfg = pf_host_config::config();
    // Both knobs off is today's host exactly: nothing read, nothing spawned, nothing logged.
    if cfg.steam_prewarm == 0 || !cfg.steam_seat_home {
        return;
    }
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("pf1-prewarm".into())
        .spawn(move || {
            run(why);
            RUNNING.store(false, Ordering::SeqCst);
        });
    if let Err(e) = spawned {
        RUNNING.store(false, Ordering::SeqCst);
        tracing::warn!(error = %e, "seat pre-warm thread not started");
    }
}

/// Park up to the cap, most recently played seat first.
fn run(why: &'static str) {
    let cap = pf_host_config::config().steam_prewarm as usize;
    let mut parked = crate::vdisplay::registry::parked_isolations();
    if parked.len() >= cap {
        return;
    }
    if let Some(pin) = pf_host_config::config().compositor.as_deref() {
        tracing::info!(
            why,
            pin,
            "seat pre-warm skipped — PUNKTFUNK_COMPOSITOR pins this host to one backend, so a \
             connect would not land in its seat's own gamescope"
        );
        return;
    }
    // Host-wide, and resolving it walks `/proc`: the route every dedicated launch on this box
    // takes, whichever seat asks. Only a spawn of its own can be stood up ahead of a connect.
    let route = crate::vdisplay::resolve_gamescope_route(Compositor::Gamescope, true);
    if !matches!(route, Some(GamescopeRoute::Spawn))
        || !crate::compositor_route::session_is_isolated(Compositor::Gamescope, route.as_ref())
    {
        tracing::info!(
            why,
            ?route,
            "seat pre-warm skipped — this host's gamescope is shared, not one per seat"
        );
        return;
    }
    for (id, rec) in candidates(crate::clock::unix_secs(), all_records()) {
        if parked.len() >= cap {
            break;
        }
        match park_seat(&id, &rec, route.clone(), &parked) {
            Ok(Some(key)) => {
                tracing::info!(
                    why,
                    seat = %id,
                    w = rec.width,
                    h = rec.height,
                    hz = rec.refresh_hz,
                    "seat pre-warmed — its Steam is up and waiting for this device"
                );
                parked.push(key);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(
                seat = %id, error = %format!("{e:#}"),
                "seat not pre-warmed"
            ),
        }
    }
}

/// Stand this seat's gamescope + Steam up. `Ok(None)` when the seat is not one to pre-warm; the
/// isolation key of the parked display otherwise.
fn park_seat(
    id: &str,
    rec: &SeatRecord,
    route: Option<GamescopeRoute>,
    parked: &[String],
) -> anyhow::Result<Option<String>> {
    let Some(fp) = fp_bytes(&rec.fingerprint).filter(|_| seat_id(&rec.fingerprint) == id) else {
        tracing::info!(seat = %id, "seat pre-warm skipped — its record names no device");
        return Ok(None);
    };
    if crate::vdisplay::admission::has_live_session(fp) {
        tracing::info!(seat = %id, "seat pre-warm skipped — this device is streaming");
        return Ok(None);
    }
    // The policy half of the route: `game_session` under this device's own overlay. A device
    // whose launches land in the box's session has nothing of its own to warm.
    if !crate::vdisplay::wants_dedicated_game_session(true, Some(fp), false) {
        tracing::info!(seat = %id, "seat pre-warm skipped — this device's launches do not get a session of their own");
        return Ok(None);
    }
    // Exclusive darkens the box's own panel for the length of the display, and a parked seat has
    // no stream to justify that. Its connect spawns cold and darkens then, as it does today.
    if crate::vdisplay::effective_topology(Some(fp)) == crate::vdisplay::policy::Topology::Exclusive
    {
        tracing::info!(seat = %id, "seat pre-warm skipped — this device blanks the box's screen while it streams");
        return Ok(None);
    }
    let iso = session_isolation(id, true);
    // Only a seat home is pre-warmed: a Steam on the box's own home is the one the player uses.
    if iso.steam_home.is_none() {
        return Ok(None);
    }
    // The registry's own key, so a seat is never parked twice under two spellings.
    let key = iso.key();
    if parked.contains(&key) {
        return Ok(None);
    }
    let mut vd = crate::vdisplay::open(Compositor::Gamescope)?;
    // A `PUNKTFUNK_CAPTURE_MONITOR` pin opens the mirror backend instead, which has no session
    // of its own to warm.
    if vd.name() != "gamescope" {
        tracing::info!(seat = %id, backend = vd.name(), "seat pre-warm skipped — this host streams a physical monitor");
        return Ok(None);
    }
    vd.set_client_identity(Some(fp));
    vd.set_hdr(rec.hdr);
    vd.set_hw_cursor(rec.hw_cursor);
    vd.set_launch_command(Some(PREWARM_LAUNCH.to_string()));
    vd.set_gamescope_route(route);
    vd.set_session_isolation(Some(iso));
    Ok(crate::vdisplay::registry::park(&mut vd, rec.mode())?.then_some(key))
}

/// Seats worth pre-warming: a Steam launch inside [`WINDOW_SECS`], most recent first. A stamp
/// ahead of `now` is a stepped clock, not a reason to forget the seat.
fn candidates(now: i64, mut seats: Vec<(String, SeatRecord)>) -> Vec<(String, SeatRecord)> {
    seats.retain(|(_, r)| now.saturating_sub(r.last_steam_launch) <= WINDOW_SECS);
    seats.sort_by_key(|(_, r)| std::cmp::Reverse(r.last_steam_launch));
    seats
}

/// Every seat record on this host. A directory that is not there yet is a host with no seats.
fn all_records() -> Vec<(String, SeatRecord)> {
    let Ok(dir) = std::fs::read_dir(pf_paths::seats_dir()) else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let id = name.to_str()?.strip_suffix(".json")?.to_string();
            Some((id, read(&e.path())?))
        })
        .collect()
}

/// The record at `path`, or `None`. A file we cannot read or parse is not an error: that seat is
/// simply not pre-warmed until its next Steam launch writes it again.
fn read(path: &Path) -> Option<SeatRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// [`pf_paths::replace_file`], so a half-written record never reads as a seat at the wrong
/// mode — the one shape the registry would refuse to hand its parked display back for.
fn write(id: &str, rec: &SeatRecord) -> anyhow::Result<()> {
    use anyhow::Context;
    pf_paths::create_private_dir(&pf_paths::seats_dir()).context("create the seats directory")?;
    pf_paths::replace_file(&pf_paths::seat_record(id), &serde_json::to_vec(rec)?)
        .context("replace the seat record")
}

/// The 32-byte fingerprint a record names. `None` on anything that is not 64 hex digits.
fn fp_bytes(hex: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(hex, &mut out).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(fp: &str, at: i64) -> SeatRecord {
        SeatRecord {
            fingerprint: fp.to_string(),
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            hdr: false,
            hw_cursor: true,
            last_steam_launch: at,
        }
    }

    /// A record survives the trip to disk, and anything else on that path reads as no record at
    /// all — a seat that is not pre-warmed, never a failed host start.
    #[test]
    fn a_seat_record_round_trips_and_a_bad_one_reads_as_none() {
        let dir = std::env::temp_dir().join(format!("pf-seat-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cafe0123.json");
        let written = rec(&"ab".repeat(32), 1_700_000_000);
        std::fs::write(&path, serde_json::to_vec(&written).unwrap()).unwrap();
        assert_eq!(read(&path).as_ref(), Some(&written));

        std::fs::write(&path, b"{\"fingerprint\":").unwrap();
        assert_eq!(read(&path), None, "a torn record is no record");
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(read(&path), None, "a record with no mode is no record");
        assert_eq!(read(&dir.join("gone.json")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only devices that played recently, newest first — the cap then takes the head of the list.
    #[test]
    fn candidates_are_recent_players_newest_first() {
        let now = 1_700_000_000;
        let seats = vec![
            ("aaaa".into(), rec("aa", now - 3600)),
            ("bbbb".into(), rec("bb", now - WINDOW_SECS - 1)),
            ("cccc".into(), rec("cc", now - 60)),
            ("dddd".into(), rec("dd", now + 600)),
        ];
        let picked: Vec<String> = candidates(now, seats)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            picked,
            ["dddd", "cccc", "aaaa"],
            "a seat outside the window is dropped; a stepped clock is not"
        );
    }

    /// The parked display and that device's session must ask the registry for one reuse key, or
    /// the pre-warm spawns a Steam nothing ever claims.
    #[test]
    fn a_parked_seat_asks_for_the_same_display_its_session_will() {
        let fp_hex = "ab".repeat(32);
        let id = seat_id(&fp_hex);
        assert_eq!(id, "abababab", "a seat is the first 8 digits of its device");
        assert_eq!(session_isolation(&id, true), session_isolation(&id, true));
        // The pre-warm launches the Steam client, which is what puts the seat's home on the
        // spawn — a command that is not Steam's would get the box's home instead.
        assert!(crate::vdisplay::launch_is_steam(PREWARM_LAUNCH));
        assert_eq!(
            fp_bytes(&fp_hex),
            Some([0xabu8; 32]),
            "the record's own device"
        );
        assert_eq!(fp_bytes("abc"), None);
    }
}
