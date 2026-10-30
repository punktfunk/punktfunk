//! Profiles on a box: what `GET /api/v1/profiles/enumerate` lists, when a client shows the
//! picker, and which id its `ClientHello` carries (`design/profiles-and-seats.md` §10).
//!
//! [`picker_decision`] is the one rule every shell follows. `clients/shared/
//! profile-picker-vectors.json` pins it here, in Swift and in Kotlin. The saved pick is
//! [`crate::trust::KnownHost::profile`]: shown on the host card, never applied unseen.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The pick a device saves for a box. Shown on the card, so it carries the name and the
/// picture's URL.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilePick {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    /// Host-relative URL of the picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// The player asked this device to remember the pick. The console applies only such a
    /// pick and asks otherwise; the other shells apply any saved pick.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remember: bool,
}

/// A seat profile's state right now. A word the host adds later reads as [`SeatState::Other`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatState {
    #[default]
    Ready,
    Starting,
    Stopped,
    Occupied,
    Unavailable,
    #[serde(other)]
    Other,
}

/// A profile's own seat. Absent for one that plays on the box's own session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Seat {
    pub state: SeatState,
    /// The progress line while starting, or why while unavailable.
    pub detail: Option<String>,
    /// The device playing on it while occupied.
    pub occupant: Option<String>,
    /// Its Steam has no account yet. `None` where the host can't tell.
    pub steam_sign_in: Option<bool>,
}

/// One row of `enumerate`. Every field defaults, so a host that adds one never fails the list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ListedProfile {
    pub id: String,
    pub display_name: String,
    /// `#RRGGBB` behind the initials.
    pub accent: Option<String>,
    /// Host-relative URL of the picture.
    pub avatar: Option<String>,
    pub owner: bool,
    pub seat: Option<Seat>,
    /// This device's old seat became this profile.
    pub legacy_seat: bool,
}

impl ListedProfile {
    pub fn pick(&self) -> ProfilePick {
        ProfilePick {
            id: self.id.clone(),
            display_name: self.display_name.clone(),
            avatar: self.avatar.clone(),
            remember: false,
        }
    }

    /// The one line under a picker card, if any (§10.2).
    pub fn note(&self) -> Option<String> {
        let seat = self.seat.as_ref()?;
        match seat.state {
            SeatState::Occupied => Some(match &seat.occupant {
                Some(device) => format!("In use by {device}"),
                None => "In use".into(),
            }),
            SeatState::Starting => Some(
                seat.detail
                    .clone()
                    .unwrap_or_else(|| "Getting ready…".into()),
            ),
            SeatState::Unavailable => {
                Some(seat.detail.clone().unwrap_or_else(|| "Unavailable".into()))
            }
            _ if seat.steam_sign_in == Some(true) => Some("Steam sign-in once".into()),
            _ => None,
        }
    }
}

/// Initials for a profile without a picture: the first letters of its first two words.
pub fn initials(name: &str) -> String {
    name.split_whitespace()
        .take(2)
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect()
}

/// The profile `wanted` names in `listed`: its id, else its name in any case.
pub fn find<'a>(listed: &'a [ListedProfile], wanted: &str) -> Option<&'a ListedProfile> {
    listed.iter().find(|p| p.id == wanted).or_else(|| {
        let mut named = listed
            .iter()
            .filter(|p| p.display_name.eq_ignore_ascii_case(wanted));
        // Two profiles can't share a name, but a hand-edited file could: refuse to guess.
        named.next().filter(|_| named.next().is_none())
    })
}

/// What a client does before it dials.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decision {
    /// Show the picker; the connect waits for a pick.
    pub picker: bool,
    /// The id the `ClientHello` carries. `None` sends none.
    pub send: Option<String>,
    /// The name of a remembered profile the box no longer lists, for the picker's line.
    pub gone: Option<String>,
    /// The saved pick afterwards.
    pub remember: Option<ProfilePick>,
}

/// §10.1. `listed` is `None` for a box without profiles. `link` is a link's or a command's
/// `as=`: it wins for this connect and leaves the saved pick alone. A kept pick keeps its
/// `remember`.
pub fn picker_decision(
    listed: Option<&[ListedProfile]>,
    remembered: Option<&ProfilePick>,
    link: Option<&str>,
) -> Decision {
    let Some(listed) = listed else {
        return Decision {
            send: link.map(str::to_string),
            remember: remembered.cloned(),
            ..Decision::default()
        };
    };
    let still = remembered.and_then(|r| {
        let p = listed.iter().find(|p| p.id == r.id)?;
        Some((
            p,
            ProfilePick {
                remember: r.remember,
                ..p.pick()
            },
        ))
    });
    let kept = || still.as_ref().map(|(_, pick)| pick.clone());
    if let Some(wanted) = link {
        return match find(listed, wanted) {
            Some(p) => Decision {
                send: Some(p.id.clone()),
                remember: kept(),
                ..Decision::default()
            },
            None => Decision {
                picker: true,
                remember: kept(),
                ..Decision::default()
            },
        };
    }
    if let [only] = listed {
        return Decision {
            send: Some(only.id.clone()),
            remember: kept(),
            ..Decision::default()
        };
    }
    if let Some((p, _)) = &still {
        return Decision {
            send: Some(p.id.clone()),
            remember: kept(),
            ..Decision::default()
        };
    }
    if remembered.is_none() {
        if let Some(p) = listed.iter().find(|p| p.legacy_seat) {
            return Decision {
                send: Some(p.id.clone()),
                remember: Some(p.pick()),
                ..Decision::default()
            };
        }
    }
    Decision {
        picker: true,
        gone: remembered.map(|r| r.display_name.clone()),
        ..Decision::default()
    }
}

/// What a client does with a picked profile's seat before it dials (§9.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SeatGate {
    /// Dial now. The host places the connect, or says why it can't.
    Dial,
    /// `POST /api/v1/profiles/{id}/wake`, then wait: `Getting Kid's desk ready…`.
    Wake,
    /// The seat is coming up. Poll `enumerate` every 2 s, show `detail`, offer **Cancel**; there
    /// is no timeout.
    Wait { detail: Option<String> },
    /// The seat can't play now; the line says why. Don't dial.
    Refuse(String),
}

/// How often a client re-reads the list while a seat comes up.
pub const SEAT_POLL: Duration = Duration::from_secs(2);

/// The gate for `p`'s seat, as `enumerate` lists it.
pub fn seat_gate(p: &ListedProfile) -> SeatGate {
    let Some(seat) = &p.seat else {
        return SeatGate::Dial;
    };
    match seat.state {
        SeatState::Ready | SeatState::Occupied | SeatState::Other => SeatGate::Dial,
        SeatState::Stopped => SeatGate::Wake,
        SeatState::Starting => SeatGate::Wait {
            detail: seat.detail.clone(),
        },
        SeatState::Unavailable => SeatGate::Refuse(
            seat.detail
                .clone()
                .unwrap_or_else(|| "That profile can't play on this host right now.".into()),
        ),
    }
}

/// The line while a seat comes up: `Getting Kid's desk ready…`.
pub fn waking_line(display_name: &str) -> String {
    format!("Getting {display_name}'s desk ready…")
}

/// The host's refusal of another profile's seat (403), in its own words.
pub const WAKE_REFUSED: &str = "This device isn't allowed to start another profile's seat.";

/// The line a failed [`wake`] ends a seat wait with: the refusal as its own sentence, any
/// other failure under the desk it was for.
pub fn wake_failed(display_name: &str, e: &crate::library::LibraryError) -> String {
    match e {
        crate::library::LibraryError::Http(403) => WAKE_REFUSED.to_string(),
        e => format!("Couldn't wake {display_name}'s desk \u{2014} {e}"),
    }
}

/// `GET /api/v1/profiles/enumerate`. `Ok(None)` when the box has no profiles (404).
#[cfg(desktop)]
pub fn fetch_enumerate(
    addr: &str,
    mgmt_port: u16,
    stream_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Result<Option<Vec<ListedProfile>>, crate::library::LibraryError> {
    use crate::library::{agent, HostApi, LibraryError};
    let https = agent(identity, pin)?;
    let api = HostApi::new(addr, mgmt_port, stream_port, identity, pin);
    let reply = api.call(&https, "GET", "/api/v1/profiles/enumerate", None)?;
    if reply.status == 404 {
        return Ok(None);
    }
    serde_json::from_slice(&reply.ok()?)
        .map(Some)
        .map_err(|e| LibraryError::Unreachable(format!("bad JSON: {e}")))
}

/// The picture at `path`, a row's [`ListedProfile::avatar`]: [`crate::art_cache`], else the
/// host, which fills the cache. `None` leaves the initials up. Blocks.
#[cfg(desktop)]
pub fn fetch_avatar(api: &crate::library::HostApi, path: &str) -> Option<Vec<u8>> {
    // Anything but a path could name another origin, and this request carries the device's cert.
    if !path.starts_with('/') {
        return None;
    }
    let https = crate::library::agent(&api.identity, api.pin).ok()?;
    let url = format!("{}{path}", api.base());
    crate::library::resolve_art(api, &https, path, &[url], || false)
}

/// `POST /api/v1/profiles/{id}/wake`: starts a stopped seat and answers with its row, which
/// says `starting` until the seat is up. A refusal is `Http(403)` ([`wake_answer`]).
#[cfg(desktop)]
pub fn wake(
    addr: &str,
    mgmt_port: u16,
    stream_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    id: &str,
) -> Result<ListedProfile, crate::library::LibraryError> {
    use crate::library::{agent, HostApi, LibraryError};
    let https = agent(identity, pin)?;
    let api = HostApi::new(addr, mgmt_port, stream_port, identity, pin);
    let reply = api.call(&https, "POST", &format!("/api/v1/profiles/{id}/wake"), None)?;
    serde_json::from_slice(&wake_answer(reply)?)
        .map_err(|e| LibraryError::Unreachable(format!("bad JSON: {e}")))
}

/// [`Reply::ok`](crate::library::Reply::ok) for a wake. The cert lane answers an unpaired
/// device 401, so a 403 is the host refusing this device the profile's seat, not "pair first".
#[cfg(desktop)]
fn wake_answer(reply: crate::library::Reply) -> Result<Vec<u8>, crate::library::LibraryError> {
    match reply.status {
        403 => Err(crate::library::LibraryError::Http(403)),
        _ => reply.ok(),
    }
}

/// [`fetch_enumerate`] cut off at `budget`; blocks the calling thread. `None`: the read failed
/// or came late. A late read finishes on its own thread into nothing.
#[cfg(desktop)]
pub fn fetch_within(
    addr: &str,
    mgmt_port: u16,
    stream_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    budget: Duration,
) -> Option<Option<Vec<ListedProfile>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (addr, identity) = (addr.to_string(), identity.clone());
    std::thread::Builder::new()
        .name("pf-profiles-fetch".into())
        .spawn(move || {
            let _ = tx.send(fetch_enumerate(
                &addr,
                mgmt_port,
                stream_port,
                &identity,
                pin,
            ));
        })
        .ok()?;
    match rx.recv_timeout(budget) {
        Ok(Ok(listed)) => Some(listed),
        Ok(Err(e)) => {
            tracing::info!(error = %e, "profile list unavailable");
            None
        }
        Err(_) => {
            tracing::info!("profile list late");
            None
        }
    }
}

/// Follows `row`'s seat on the calling thread: [`wake`]s a stopped seat first, then hands
/// `each` that row and every row `enumerate` lists for it, one per [`SEAT_POLL`]. Ends when
/// `each` returns `false`, when `stop` is set after a sleep, or after an `Err`: the line that
/// ends the wait (the wake failed, or the host no longer lists the profile). A failed read
/// waits for the next.
#[cfg(desktop)]
#[allow(clippy::too_many_arguments)]
pub fn watch_seat(
    addr: &str,
    mgmt_port: u16,
    stream_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    row: &ListedProfile,
    stop: &std::sync::atomic::AtomicBool,
    mut each: impl FnMut(Result<ListedProfile, String>) -> bool,
) {
    let name = &row.display_name;
    let mut row = row.clone();
    if seat_gate(&row) == SeatGate::Wake {
        match wake(addr, mgmt_port, stream_port, identity, pin, &row.id) {
            Ok(woken) => row = woken,
            Err(e) => {
                each(Err(wake_failed(name, &e)));
                return;
            }
        }
    }
    if !each(Ok(row.clone())) {
        return;
    }
    loop {
        std::thread::sleep(SEAT_POLL);
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let listed = match fetch_enumerate(addr, mgmt_port, stream_port, identity, pin) {
            Ok(listed) => listed,
            Err(e) => {
                tracing::debug!(error = %e, "seat poll");
                continue;
            }
        };
        let Some(polled) = listed.into_iter().flatten().find(|p| p.id == row.id) else {
            each(Err(format!("{name} is gone from this host.")));
            return;
        };
        if !each(Ok(polled)) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(desktop)]
    use std::sync::atomic::AtomicBool;

    #[test]
    fn a_pick_carries_the_picture_and_a_kept_pick_follows_a_new_one() {
        let row = |avatar: &str| ListedProfile {
            id: "kid".into(),
            display_name: "Kid".into(),
            avatar: Some(avatar.into()),
            ..ListedProfile::default()
        };
        let old = ProfilePick {
            remember: true,
            ..row("/p/kid/avatar?v=old").pick()
        };
        let listed = [row("/p/kid/avatar?v=new"), ListedProfile::default()];
        let kept = picker_decision(Some(&listed), Some(&old), None).remember;
        assert_eq!(
            kept,
            Some(ProfilePick {
                avatar: Some("/p/kid/avatar?v=new".into()),
                ..old.clone()
            })
        );
    }

    #[test]
    fn a_saved_pick_without_a_picture_keeps_its_old_shape() {
        let saved = ProfilePick {
            id: "kid".into(),
            display_name: "Kid".into(),
            ..ProfilePick::default()
        };
        let json = serde_json::to_value(&saved).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"id": "kid", "display_name": "Kid"})
        );
        assert_eq!(serde_json::from_value::<ProfilePick>(json).unwrap(), saved);
    }

    #[cfg(desktop)]
    #[test]
    fn a_picture_must_be_a_path_on_the_host() {
        let identity = (String::new(), String::new());
        let api = crate::library::HostApi::new("10.0.0.2", 47990, 0, &identity, None);
        assert_eq!(fetch_avatar(&api, "https://elsewhere.example/x.png"), None);
        assert_eq!(fetch_avatar(&api, "@elsewhere.example/x.png"), None);
    }

    #[test]
    fn a_seat_is_dialed_woken_waited_for_or_refused() {
        let with = |state: &str, detail: Option<&str>| -> ListedProfile {
            serde_json::from_value(serde_json::json!({
                "id": "kid", "display_name": "Kid",
                "seat": { "state": state, "detail": detail, "port": 9778 },
            }))
            .unwrap()
        };
        let none: ListedProfile = serde_json::from_value(serde_json::json!({"id": "own"})).unwrap();
        assert_eq!(seat_gate(&none), SeatGate::Dial);
        assert_eq!(seat_gate(&with("ready", None)), SeatGate::Dial);
        assert_eq!(seat_gate(&with("occupied", None)), SeatGate::Dial);
        assert_eq!(seat_gate(&with("stopped", None)), SeatGate::Wake);
        assert_eq!(
            seat_gate(&with("starting", Some("Signing in"))),
            SeatGate::Wait {
                detail: Some("Signing in".into())
            }
        );
        assert_eq!(
            seat_gate(&with("unavailable", Some("Seats are off."))),
            SeatGate::Refuse("Seats are off.".into())
        );
        assert_eq!(waking_line("Kid"), "Getting Kid's desk ready…");
    }

    /// A wake the host refuses (403) reads as its sentence; an unpaired device (401) and any
    /// other failure keep the desk's frame.
    #[test]
    fn a_refused_wake_reads_as_the_hosts_sentence() {
        use crate::library::LibraryError;
        assert_eq!(
            wake_failed("Kid", &LibraryError::Http(403)),
            "This device isn't allowed to start another profile's seat."
        );
        assert_eq!(
            wake_failed("Kid", &LibraryError::Http(500)),
            "Couldn't wake Kid's desk \u{2014} the host refused it (500)"
        );
        #[cfg(desktop)]
        {
            let answer = |status| {
                wake_answer(crate::library::Reply {
                    status,
                    body: Vec::new(),
                })
            };
            let refused = answer(403);
            assert!(
                matches!(refused, Err(LibraryError::Http(403))),
                "{refused:?}"
            );
            let unpaired = answer(401);
            assert!(
                matches!(unpaired, Err(LibraryError::NotPaired)),
                "{unpaired:?}"
            );
        }
    }

    #[derive(Deserialize)]
    struct Vectors {
        cases: Vec<Case>,
    }

    #[derive(Deserialize)]
    struct Case {
        name: String,
        listed: Option<Vec<ListedProfile>>,
        remembered: Option<ProfilePick>,
        link: Option<String>,
        expect: Expect,
    }

    #[derive(Deserialize)]
    struct Expect {
        picker: bool,
        send: Option<String>,
        gone: Option<String>,
        remember: Option<String>,
    }

    #[test]
    fn profiles_follow_the_shared_picker_vectors() {
        let raw = include_str!("../../../../clients/shared/profile-picker-vectors.json");
        let v: Vectors = serde_json::from_str(raw).expect("vectors parse");
        assert!(!v.cases.is_empty());
        for c in v.cases {
            let d = picker_decision(
                c.listed.as_deref(),
                c.remembered.as_ref(),
                c.link.as_deref(),
            );
            assert_eq!(d.picker, c.expect.picker, "{}: picker", c.name);
            assert_eq!(d.send, c.expect.send, "{}: send", c.name);
            assert_eq!(d.gone, c.expect.gone, "{}: gone", c.name);
            assert_eq!(
                d.remember.map(|p| p.id),
                c.expect.remember,
                "{}: remember",
                c.name
            );
        }
    }

    /// A kept pick keeps `remember`, so a shell that saves `remember` back never clears it.
    #[test]
    fn a_kept_pick_keeps_its_remember() {
        let listed: Vec<ListedProfile> = serde_json::from_value(serde_json::json!([
            {"id": "own", "display_name": "Ben"},
            {"id": "kid", "display_name": "Kid"},
        ]))
        .unwrap();
        let saved = ProfilePick {
            id: "kid".into(),
            display_name: "Old name".into(),
            remember: true,
            ..ProfilePick::default()
        };
        let d = picker_decision(Some(&listed), Some(&saved), None);
        let kept = d.remember.expect("still listed");
        assert!(kept.remember);
        assert_eq!(kept.display_name, "Kid");
        let old = ProfilePick {
            remember: false,
            ..saved
        };
        let d = picker_decision(Some(&listed), Some(&old), None);
        assert_eq!(d.remember.map(|p| p.remember), Some(false));
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("remember"), "{json}");
    }

    #[test]
    fn profiles_decode_a_host_row_and_word_its_seat() {
        let raw = r##"[{"id":"9a3f1c2b7e40","display_name":"Kid","accent":"#f97316",
            "owner":false,"home":"bigpicture","last_used_unix":0,"future":1,
            "seat":{"state":"occupied","port":9777,"occupant":"Ben's Apple TV"}},
            {"id":"x","display_name":"Odd","seat":{"state":"sleeping","port":1}}]"##;
        let rows: Vec<ListedProfile> = serde_json::from_str(raw).unwrap();
        assert_eq!(rows[0].note().as_deref(), Some("In use by Ben's Apple TV"));
        assert_eq!(rows[1].seat.as_ref().unwrap().state, SeatState::Other);
        assert_eq!(initials("anna lena x"), "AL");
    }

    #[cfg(desktop)]
    #[test]
    fn a_seat_watch_hands_on_the_first_row_or_the_wake_failure() {
        let row = |state: &str| -> ListedProfile {
            serde_json::from_value(serde_json::json!({
                "id": "kid", "display_name": "Kid", "seat": { "state": state },
            }))
            .unwrap()
        };
        // An empty identity fails every request before it leaves this machine.
        let (bad, stop) = ((String::new(), String::new()), AtomicBool::new(false));
        let mut seen = Vec::new();
        watch_seat("127.0.0.1", 1, 0, &bad, None, &row("stopped"), &stop, |r| {
            seen.push(r);
            true
        });
        assert!(
            matches!(&seen[..], [Err(l)] if l.starts_with("Couldn't wake Kid's desk \u{2014} "))
        );
        seen.clear();
        watch_seat(
            "127.0.0.1",
            1,
            0,
            &bad,
            None,
            &row("starting"),
            &stop,
            |r| {
                seen.push(r);
                false
            },
        );
        assert_eq!(seen, vec![Ok(row("starting"))]);
    }
}
