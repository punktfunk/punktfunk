//! Placement: on a box with seats, a seat profile plays on its seat's host, so the box answers
//! that profile's connect with a `Redirect` to the seat's port, or a refusal saying why.
//!
//! The box resolves the profile first (`profiles.rs`); placement only routes. The owner, a
//! profile that shares the box's desktop and a Linux light seat play on the box, or on the
//! owner's row when the box is a door, which streams nothing itself. A device the
//! box redirects holds its seat for [`RESERVE`], so a second device racing in finds the seat
//! taken before the first lands. A device whose overlay says **A second device connects →
//! Shares the screen** is redirected to an occupied seat too; the seat host's admission joins it.

use super::Occupant;
use pf_seats::{RuntimeState, Seat};
use punktfunk_core::quic::v2::msg::Redirect;
use punktfunk_core::reject::RejectReason;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a redirected device holds its seat before it lands there.
const RESERVE: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    /// The box's own session.
    Here,
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
}

/// Where `profile` plays for `asker`. Blocking (the supervisor's pipe and the seat's loopback
/// API): call it off the async workers.
///
/// A door streams nothing itself, so it places every profile: the owner, a profile that shares
/// the owner's desktop and a light seat on the owner's row, a full seat on its own. A profile no
/// row serves, because the box has no owner row yet, is refused rather than played here.
pub(crate) fn place(profile: &crate::profiles::Resolved, asker: &Asker) -> Placement {
    // A seat host plays the profile it was asked for: resolving it already checked it is this
    // seat's, and a seat has no ledger to place anything in.
    if pf_paths::seat::is_seat_host()
        || (!super::is_door() && ledger_seat(&profile.os_account).is_none())
    {
        return Placement::Here;
    }
    let snap = super::snapshot();
    let Some(seat_id) = snap.row_of(&profile.os_account) else {
        return Placement::Refuse(RejectReason::SeatUnavailable);
    };
    let occupants = snap.occupants.get(seat_id).map_or(&[][..], Vec::as_slice);
    let mut held = RESERVED.lock().unwrap_or_else(|e| e.into_inner());
    held.retain(|_, (_, until)| *until > Instant::now());
    let held_by = held.get(seat_id).map(|(fp, _)| fp.as_str());
    let placed = decide(
        &profile.id,
        asker,
        snap.on,
        snap.seat(seat_id),
        occupants,
        held_by,
    );
    if let (Placement::Redirect(_), Some(fp)) = (&placed, asker.fp) {
        held.insert(
            seat_id.to_string(),
            (fp.to_string(), Instant::now() + RESERVE),
        );
    }
    placed
}

/// Seat id → the device redirected to it and until when it holds it.
static RESERVED: Mutex<BTreeMap<String, (String, Instant)>> = Mutex::new(BTreeMap::new());

/// A profile whose seat is a ledger row of its own. A light seat names none and plays on the
/// box's own host, or on the owner's behind a door.
fn ledger_seat(account: &crate::profiles::OsAccount) -> Option<&str> {
    match account {
        crate::profiles::OsAccount::Seat { seat: Some(id), .. } => Some(id),
        _ => None,
    }
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
    Placement::Redirect(Redirect {
        addr: String::new(),
        port: seat.native_port,
        profile: profile_id.to_string(),
        seat_no,
        seat_name: seat.name.clone(),
        occupant: other.and_then(|o| o.name.clone()).unwrap_or_default(),
        pin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "aa11bb22cc33dd44ee55ff6600000000000000000000000000000000000000aa";
    const THEM: &str = "ff99ee88dd77cc66bb55aa4400000000000000000000000000000000000000ff";

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
        }
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
