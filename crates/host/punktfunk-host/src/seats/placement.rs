//! Placement: on a box with seats, a seat profile plays on its seat's host, so the box answers
//! that profile's connect with a `Redirect` to the seat's port, or a refusal saying why.
//!
//! The box resolves the profile first (`profiles.rs`); placement only routes, and a person's own
//! Linux account only once the supervisor unlocked it (`native/unlock.rs`). The owner and a
//! profile that shares the box's desktop play on the box, or on the owner's row when the box is
//! a door, which streams nothing itself. A device the box redirects holds its seat until the seat
//! lists it, at most [`LANDING`], so a second device racing in finds the seat taken before the
//! first lands. A device whose overlay says **A second device connects → Shares the screen** is redirected to
//! an occupied seat too; the seat host's admission joins it.

use super::Occupant;
use pf_paths::seat::HostRole;
use pf_seats::{RuntimeState, Seat};
use punktfunk_core::quic::v2::msg::Redirect;
use punktfunk_core::reject::RejectReason;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a redirected device may take to be listed on its seat: it dials, then the seat brings
/// the picture up before it lists the session. The wait `native.rs`'s `SEAT_LANDING` gives a
/// redirected guest.
const LANDING: Duration = Duration::from_secs(120);

/// How often a held seat's host is asked whether its device is listed yet.
const LANDING_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    Redirect(Redirect),
    Refuse(RejectReason),
}

/// The device asking, as placement needs it.
pub(crate) struct Asker<'a> {
    /// Lowercase hex; `None` for an anonymous client.
    pub fp: Option<&'a str>,
    /// It set `PROFILES`, so it reads and follows a `Redirect`.
    pub follows_redirects: bool,
    /// Its overlay on the box joins a live session instead of refusing.
    pub joins: bool,
    /// A browser: it lands on the seat's browser plane, not its native port.
    pub browser: bool,
}

/// Whether a connect for `account` plays on the host that was asked, the box's own session,
/// rather than being placed on a seat.
pub(crate) fn here(account: &crate::profiles::OsAccount) -> bool {
    plays_here(pf_paths::seat::role(), ledger_seat(account).is_some())
}

/// Where `profile` plays for `asker`, one that isn't [`here`], by the ledger `snap`. Blocking
/// (the seat's loopback API): call it off the async workers.
///
/// A door streams nothing itself, so it places every profile: the owner and a profile that
/// shares the owner's desktop on the owner's row, a full seat on its own. A profile no row
/// serves, because the box has no owner row yet, is refused rather than played here. Only the
/// profile's own seat host is asked, and never under [`RESERVED`]: a hung seat host costs this
/// connect alone.
pub(crate) fn place(
    profile: &crate::profiles::Resolved,
    asker: &Asker,
    snap: &super::Snapshot,
) -> Placement {
    let Some(seat_id) = snap.row_of(&profile.os_account) else {
        return Placement::Refuse(RejectReason::SeatUnavailable);
    };
    let running = snap
        .seat(seat_id)
        .filter(|(seat, _)| seat.runtime.state == RuntimeState::Running);
    let occupants = running
        .and_then(|(seat, _)| super::occupants(seat))
        .unwrap_or_default();
    let plane = running
        .filter(|_| asker.browser)
        .and_then(|(seat, _)| browser_plane(seat));
    let mut held = RESERVED.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let held_by = live_holder(&mut held, seat_id, now).map(str::to_owned);
    let placed = decide(
        &profile.id,
        asker,
        snap.on,
        snap.seat(seat_id),
        &occupants,
        held_by.as_deref(),
    );
    let placed = match placed {
        Placement::Redirect(to) if asker.browser => with_browser_plane(to, plane),
        placed => placed,
    };
    if matches!(placed, Placement::Redirect(_)) {
        PLACED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seat_id.to_string(), Instant::now());
        if let Some(fp) = asker.fp
            && hold(&mut held, seat_id, fp, now)
            && let Some((seat, _)) = snap.seat(seat_id)
        {
            watch_landing(seat.clone());
        }
    }
    placed
}

/// Seat id → the device redirected to it and until when it holds it.
type Holds = BTreeMap<String, (String, Instant)>;

/// The device holding `seat_id` at `now`. Lapsed holds go.
fn live_holder<'a>(held: &'a mut Holds, seat_id: &str, now: Instant) -> Option<&'a str> {
    held.retain(|_, (_, until)| *until > now);
    held.get(seat_id).map(|(fp, _)| fp.as_str())
}

/// Holds `seat_id` for `fp` until `now` + [`LANDING`]. `true` when no hold was live, so nothing
/// watches the seat yet.
fn hold(held: &mut Holds, seat_id: &str, fp: &str, now: Instant) -> bool {
    let fresh = live_holder(held, seat_id, now).is_none();
    held.insert(seat_id.to_string(), (fp.to_string(), now + LANDING));
    fresh
}

/// Lets `seat_id`'s hold go when `fp` still holds it.
fn release(held: &mut Holds, seat_id: &str, fp: &str) {
    if held.get(seat_id).is_some_and(|(holder, _)| holder == fp) {
        held.remove(seat_id);
    }
}

/// Asks `seat`'s host every [`LANDING_POLL`] whether the device holding it is listed, and lets
/// the hold go once it is: from then on the seat's own occupants keep it, and a device that
/// lands and leaves frees the seat. Ends with the hold.
fn watch_landing(seat: Seat) {
    let spawned = std::thread::Builder::new()
        .name("seat-landing".into())
        .spawn(move || {
            let id = seat.id.as_str();
            loop {
                std::thread::sleep(LANDING_POLL);
                let fp = {
                    let mut held = RESERVED.lock().unwrap_or_else(|e| e.into_inner());
                    match live_holder(&mut held, id, Instant::now()) {
                        Some(fp) => fp.to_owned(),
                        None => return,
                    }
                };
                let listed = super::occupants(&seat).is_some_and(|there| {
                    there
                        .iter()
                        .any(|o| crate::session_status::is_client(&o.client, &fp))
                });
                if listed {
                    let mut held = RESERVED.lock().unwrap_or_else(|e| e.into_inner());
                    release(&mut held, id, &fp);
                    return;
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "seat landing watch did not start");
    }
}

/// `seat`'s browser plane as its host's `/webtransport` answers, `None` when it doesn't. Blocking.
fn browser_plane(seat: &Seat) -> Option<serde_json::Value> {
    let call = crate::mgmt::owner::Forwarded::get("webtransport".into());
    match super::forward(seat, call) {
        Some(crate::mgmt::owner::Answer::Reply(200, _, body)) => serde_json::from_slice(&body).ok(),
        _ => None,
    }
}

/// `to` with the certificate hash its seat's browser `plane` serves under, which the browser
/// pins and binds its credential to. A plane that didn't answer on the port the redirect names,
/// as one started before the box streamed to browsers, is unavailable.
fn with_browser_plane(mut to: Redirect, plane: Option<serde_json::Value>) -> Placement {
    let hash = plane
        .filter(|p| p["port"].as_u64() == Some(u64::from(to.port)))
        .and_then(|p| p["cert_hash_sha256"].as_str().map(str::to_ascii_lowercase));
    match hash {
        Some(hash) => {
            to.cert_hash = hash;
            Placement::Redirect(to)
        }
        None => Placement::Refuse(RejectReason::SeatUnavailable),
    }
}

/// Seat id → when a device was last redirected to it.
static PLACED: Mutex<BTreeMap<String, Instant>> = Mutex::new(BTreeMap::new());

/// When a device was last redirected to `seat_id`. A seat's idle clock starts no earlier: a
/// player who lands and leaves between two of its checks still counts.
#[cfg(any(windows, target_os = "linux"))]
pub(crate) fn last_placed(seat_id: &str) -> Option<Instant> {
    PLACED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(seat_id)
        .copied()
}

/// Whether the host that was asked plays the profile itself. A seat host does: resolving the
/// profile already checked it is this seat's, and a seat has no ledger to place anything in.
/// A door never does. Any other host places only a profile with a ledger row of its own.
fn plays_here(role: HostRole, own_row: bool) -> bool {
    match role {
        HostRole::Seat(_) => true,
        HostRole::Door => false,
        HostRole::Plain => !own_row,
    }
}

static RESERVED: Mutex<Holds> = Mutex::new(BTreeMap::new());

/// A profile whose seat is a ledger row of its own: the row [`row_in`](super::row_in) names
/// with no owner row, so the owner's profile has none.
fn ledger_seat(account: &crate::profiles::OsAccount) -> Option<&str> {
    super::row_in(account, None, false)
}

/// The rule, without IO. `seat` is the profile's ledger row and its seat number, `occupants`
/// who streams there, `held_by` a live reservation's device. A seat whose host pin the ledger
/// doesn't hold yet is unavailable: a client sent there with the box's pin would fail.
fn decide(
    profile_id: &str,
    asker: &Asker,
    seats_on: bool,
    seat: Option<(&Seat, u8)>,
    occupants: &[Occupant],
    held_by: Option<&str>,
) -> Placement {
    if !seats_on {
        return Placement::Refuse(RejectReason::SeatUnavailable);
    }
    // A client that can't follow a redirect can't reach any seat.
    if !asker.follows_redirects {
        return Placement::Refuse(RejectReason::NoSeat);
    }
    let Some((seat, seat_no)) = seat else {
        return Placement::Refuse(RejectReason::SeatUnavailable);
    };
    let Some(pin) = seat
        .fingerprint
        .clone()
        .filter(|_| seat.runtime.state == RuntimeState::Running)
    else {
        return Placement::Refuse(RejectReason::SeatUnavailable);
    };
    let mine =
        |client: &str| !client.is_empty() && asker.fp.is_some_and(|fp| fp.starts_with(client));
    let other = occupants.iter().find(|o| !mine(&o.client));
    let held_by_other = held_by.is_some_and(|fp| asker.fp != Some(fp));
    if (other.is_some() || held_by_other) && !asker.joins {
        return Placement::Refuse(RejectReason::SeatOccupied);
    }
    let port = if asker.browser {
        match seat.browser_port() {
            Some(port) => port,
            None => return Placement::Refuse(RejectReason::SeatUnavailable),
        }
    } else {
        seat.native_port
    };
    Placement::Redirect(Redirect {
        addr: String::new(),
        port,
        profile: profile_id.to_string(),
        seat_no,
        seat_name: seat.name.clone(),
        occupant: other.and_then(|o| o.name.clone()).unwrap_or_default(),
        pin,
        cert_hash: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "aa11bb22cc33dd44ee55ff6600000000000000000000000000000000000000aa";
    const THEM: &str = "ff99ee88dd77cc66bb55aa4400000000000000000000000000000000000000ff";

    #[test]
    fn a_seat_host_plays_what_it_is_asked_and_a_door_places_it() {
        for role in HostRole::ALL {
            for own_row in [false, true] {
                let asked = role.is_seat_host() || (!role.is_door() && !own_row);
                assert_eq!(
                    plays_here(role, own_row),
                    asked,
                    "{role:?}, own row {own_row}"
                );
            }
        }
    }

    /// The door's owner row is the owner's, never a profile's own: placement reads one mapping.
    #[test]
    fn only_a_full_seat_has_a_ledger_row_of_its_own() {
        use crate::profiles::OsAccount::{Operator, Seat as Full};
        let row = "0123456789abcdef0123456789abcdef";
        let full = Full {
            seat: Some(row.into()),
        };
        assert_eq!(ledger_seat(&full), Some(row));
        assert_eq!(ledger_seat(&Full { seat: None }), None);
        assert_eq!(ledger_seat(&Operator), None);
        let mut owner = seat(RuntimeState::Running);
        owner.owner = true;
        assert_eq!(
            crate::seats::row_in(&Operator, Some(&owner), true),
            Some(row)
        );
        assert_eq!(crate::seats::row_in(&full, Some(&owner), true), Some(row));
    }

    fn seat(state: RuntimeState) -> Seat {
        serde_json::from_value(serde_json::json!({
            "id": "0123456789abcdef0123456789abcdef",
            "name": "Kid",
            "account": "pf_seat1",
            "display_slot": 12,
            "native_port": 9778,
            "mgmt_port": 47991,
            "runtime": { "state": state },
            "fingerprint": PIN,
        }))
        .unwrap()
    }

    const PIN: &str = "5eed5eed00000000000000000000000000000000000000000000000000005eed";

    /// The redirect carries the seat host's own pin; a running seat whose pin the ledger doesn't
    /// hold yet is unavailable rather than sent with the box's.
    #[test]
    fn a_redirect_pins_the_seat_host() {
        let mut s = seat(RuntimeState::Running);
        let Placement::Redirect(r) = decide("kid", &asker(true, false), true, on(&s), &[], None)
        else {
            panic!("a running seat is redirected to");
        };
        assert_eq!(r.pin, PIN);
        s.fingerprint = None;
        assert_eq!(
            decide("kid", &asker(true, false), true, on(&s), &[], None),
            Placement::Refuse(RejectReason::SeatUnavailable)
        );
    }

    /// A door's owner row is the same row to `decide`: the redirect names its port and the
    /// profile, and a stopped owner is `SeatUnavailable`, which a client answers with a wake.
    #[test]
    fn the_owner_row_is_placed_like_any_seat() {
        let mut owner = seat(RuntimeState::Running);
        owner.owner = true;
        let Placement::Redirect(r) =
            decide("owner", &asker(true, false), true, on(&owner), &[], None)
        else {
            panic!("the owner is redirected");
        };
        assert_eq!((r.port, r.profile.as_str()), (9778, "owner"));
        owner.runtime = pf_seats::RuntimeStatus::stopped();
        assert_eq!(
            decide("owner", &asker(true, false), true, on(&owner), &[], None),
            Placement::Refuse(RejectReason::SeatUnavailable)
        );
    }

    fn asker(follows: bool, joins: bool) -> Asker<'static> {
        Asker {
            fp: Some(ME),
            follows_redirects: follows,
            joins,
            browser: false,
        }
    }

    /// A browser lands on its seat's browser plane; a seat with no browser port has none to
    /// send it to.
    #[test]
    fn a_browser_goes_to_its_seat_s_browser_plane() {
        let browser = Asker {
            browser: true,
            ..asker(true, false)
        };
        let mut s = seat(RuntimeState::Running);
        s.native_port = 9780;
        let Placement::Redirect(r) = decide("kid", &browser, true, on(&s), &[], None) else {
            panic!("a running seat is redirected to");
        };
        assert_eq!((r.port, r.pin.as_str()), (9784, PIN));
        s.native_port = 9778;
        assert_eq!(
            decide("kid", &browser, true, on(&s), &[], None),
            Placement::Refuse(RejectReason::SeatUnavailable)
        );
    }

    /// The browser pins the hash its seat's plane serves on the redirect's port; a plane on
    /// another port, or none, leaves the seat unavailable.
    #[test]
    fn a_browser_redirect_carries_its_plane_s_hash() {
        let s = seat(RuntimeState::Running);
        let Placement::Redirect(to) = decide("kid", &asker(true, false), true, on(&s), &[], None)
        else {
            panic!("a running seat is redirected to");
        };
        let plane = |port: u16| Some(serde_json::json!({"port": port, "cert_hash_sha256": "AB12"}));
        let Placement::Redirect(r) = with_browser_plane(to.clone(), plane(9778)) else {
            panic!("the plane answers on the redirect's port");
        };
        assert_eq!(r.cert_hash, "ab12");
        let refused = Placement::Refuse(RejectReason::SeatUnavailable);
        assert_eq!(with_browser_plane(to.clone(), plane(9784)), refused);
        assert_eq!(with_browser_plane(to, None), refused);
    }

    fn on(seat: &Seat) -> Option<(&Seat, u8)> {
        Some((seat, 1))
    }

    fn port(p: &Placement) -> Option<u16> {
        match p {
            Placement::Redirect(r) => Some(r.port),
            _ => None,
        }
    }

    #[test]
    fn a_free_running_seat_redirects_to_its_port() {
        let s = seat(RuntimeState::Running);
        let p = decide("kid", &asker(true, false), true, on(&s), &[], None);
        assert_eq!(port(&p), Some(9778));
        let Placement::Redirect(r) = p else {
            unreachable!()
        };
        assert_eq!(
            (r.profile.as_str(), r.seat_no, r.addr.as_str()),
            ("kid", 1, "")
        );
    }

    #[test]
    fn seats_off_a_missing_row_or_a_stopped_seat_is_unavailable() {
        let s = seat(RuntimeState::Stopped);
        let refused = Placement::Refuse(RejectReason::SeatUnavailable);
        assert_eq!(
            decide("kid", &asker(true, false), false, None, &[], None),
            refused
        );
        assert_eq!(
            decide("kid", &asker(true, false), true, None, &[], None),
            refused
        );
        assert_eq!(
            decide("kid", &asker(true, false), true, on(&s), &[], None),
            refused
        );
    }

    #[test]
    fn a_client_that_cannot_follow_a_redirect_gets_no_seat() {
        let s = seat(RuntimeState::Running);
        assert_eq!(
            decide("kid", &asker(false, false), true, on(&s), &[], None),
            Placement::Refuse(RejectReason::NoSeat)
        );
    }

    #[test]
    fn another_device_on_the_seat_refuses_unless_this_one_joins() {
        let s = seat(RuntimeState::Running);
        let there = [Occupant {
            client: THEM[..16].to_string(),
            name: Some("Ben's Apple TV".into()),
        }];
        assert_eq!(
            decide("kid", &asker(true, false), true, on(&s), &there, None),
            Placement::Refuse(RejectReason::SeatOccupied)
        );
        let Placement::Redirect(r) = decide("kid", &asker(true, true), true, on(&s), &there, None)
        else {
            panic!("a joiner is redirected");
        };
        assert_eq!(r.occupant, "Ben's Apple TV");
    }

    /// A redirect holds its seat through a cold seat's start, until its device is listed there or
    /// the landing wait ends; another device's landing leaves it.
    #[test]
    fn a_hold_lasts_until_its_device_is_listed_or_the_landing_wait_ends() {
        let mut held = Holds::new();
        let now = Instant::now();
        assert!(
            hold(&mut held, "a", ME, now),
            "nothing watches a fresh hold"
        );
        assert!(!hold(&mut held, "a", ME, now), "its watch goes on");
        let starting = now + Duration::from_secs(60);
        assert_eq!(live_holder(&mut held, "a", starting), Some(ME));
        release(&mut held, "a", THEM);
        assert_eq!(live_holder(&mut held, "a", now), Some(ME));
        release(&mut held, "a", ME);
        assert_eq!(live_holder(&mut held, "a", now), None, "listed");
        hold(&mut held, "a", ME, now);
        assert_eq!(
            live_holder(&mut held, "a", now + LANDING),
            None,
            "never came"
        );
    }

    #[test]
    fn the_same_device_and_its_own_reservation_get_back_in() {
        let s = seat(RuntimeState::Running);
        let me_there = [Occupant {
            client: ME[..16].to_string(),
            name: None,
        }];
        let a = asker(true, false);
        assert_eq!(
            port(&decide("kid", &a, true, on(&s), &me_there, None)),
            Some(9778)
        );
        assert_eq!(
            port(&decide("kid", &a, true, on(&s), &[], Some(ME))),
            Some(9778)
        );
        assert_eq!(
            decide("kid", &a, true, on(&s), &[], Some(THEM)),
            Placement::Refuse(RejectReason::SeatOccupied)
        );
    }
}
