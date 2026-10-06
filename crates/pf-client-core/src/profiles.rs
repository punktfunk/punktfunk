//! Profiles on a box: what `GET /api/v1/profiles/enumerate` lists, when a client shows the
//! picker, and which id its `ClientHello` carries (`design/profiles-and-seats.md` §10).
//!
//! [`picker_decision`] is the one rule every shell follows. `clients/shared/
//! profile-picker-vectors.json` pins it here, in Swift and in Kotlin. The saved pick is
//! [`crate::trust::KnownHost::profile`]: shown on the host card, never applied unseen.

use serde::{Deserialize, Serialize};

/// The pick a device remembers for a box. Shown on the card, so it carries the name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilePick {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
}

/// A seat profile's state right now. A word the host adds later reads as [`SeatState::Other`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
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
/// `as=`: it wins for this connect and leaves the saved pick alone.
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
    let still = remembered.and_then(|r| listed.iter().find(|p| p.id == r.id));
    if let Some(wanted) = link {
        return match find(listed, wanted) {
            Some(p) => Decision {
                send: Some(p.id.clone()),
                remember: still.map(ListedProfile::pick),
                ..Decision::default()
            },
            None => Decision {
                picker: true,
                remember: still.map(ListedProfile::pick),
                ..Decision::default()
            },
        };
    }
    if let [only] = listed {
        return Decision {
            send: Some(only.id.clone()),
            remember: still.map(ListedProfile::pick),
            ..Decision::default()
        };
    }
    if let Some(p) = still {
        return Decision {
            send: Some(p.id.clone()),
            remember: Some(p.pick()),
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

/// `GET /api/v1/profiles/enumerate`. `Ok(None)` when the box has no profiles (404).
#[cfg(desktop)]
pub fn fetch_enumerate(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Result<Option<Vec<ListedProfile>>, crate::library::LibraryError> {
    use crate::library::{agent, base_url, classify, LibraryError};
    let agent = agent(identity, pin)?;
    let url = format!("{}/api/v1/profiles/enumerate", base_url(addr, mgmt_port));
    let mut resp = match agent.get(&url).call() {
        Ok(resp) => resp,
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(e) => return Err(classify(e)),
    };
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| LibraryError::Unreachable(format!("read body: {e}")))?;
    serde_json::from_str(&body)
        .map(Some)
        .map_err(|e| LibraryError::Unreachable(format!("bad JSON: {e}")))
}

/// `POST /api/v1/profiles/{id}/wake`: starts a stopped seat and answers with its row, which
/// says `starting` until the seat is up.
#[cfg(desktop)]
pub fn wake(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    id: &str,
) -> Result<ListedProfile, crate::library::LibraryError> {
    use crate::library::{agent, base_url, classify, LibraryError};
    let agent = agent(identity, pin)?;
    let url = format!("{}/api/v1/profiles/{id}/wake", base_url(addr, mgmt_port));
    let mut resp = agent.post(&url).send_empty().map_err(classify)?;
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| LibraryError::Unreachable(format!("read body: {e}")))?;
    serde_json::from_str(&body).map_err(|e| LibraryError::Unreachable(format!("bad JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let raw = include_str!("../../../clients/shared/profile-picker-vectors.json");
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
}
