//! "Hide for me": a player takes a title off the library of the locked profile this device is let
//! into, and brings it back. `enumerate` marks such a row `person`; a client offers the action on
//! the saved pick's row so marked, and on no other. A host that sends no mark has no action.
//!
//! A hide or a show is `PUT /api/v1/library/hidden/{id}?as=<profile>`. A refusal reads as the
//! host's own sentence. What the profile hides is `GET /api/v1/library/page?hidden=true&as=`,
//! fetched when asked for and never cached. `clients/shared/hide-for-me-vectors.json` pins this
//! here, in Swift and in Kotlin.

use crate::library::{query_value, walk_pages, GameEntry, LibraryError, Reply, PAGE_LIMIT};
use crate::profiles::ListedProfile;

/// The row of the profile this device plays as when it may hide titles there: `listed` is what
/// `enumerate` answered (`None`: the host has no profiles), `pick` the saved pick's id.
pub fn offer<'a>(
    listed: Option<&'a [ListedProfile]>,
    pick: Option<&str>,
) -> Option<&'a ListedProfile> {
    let pick = pick?;
    listed?.iter().find(|p| p.id == pick && p.person)
}

/// The path and JSON body of the `PUT` that hides (`hidden`) or shows title `id` for `profile`.
pub fn request(profile: &str, id: &str, hidden: bool) -> (String, String) {
    let id = crate::library::path_segment(id);
    let path = format!("/api/v1/library/hidden/{id}?as={}", query_value(profile));
    (path, format!("{{\"hidden\":{hidden}}}"))
}

/// What a hide or a show came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// The host's sentence, worded by its code where this build has the message.
    Refused(String),
    /// No answer to word: a lowercase cause.
    Failed(String),
}

impl Outcome {
    /// Only a coded error is a sentence for the player; one with no code is the operator's
    /// line and reads as a failure.
    pub fn from_reply(status: u16, body: &str) -> Outcome {
        let coded = serde_json::from_str::<serde_json::Value>(body)
            .is_ok_and(|j| j["code"].as_str().is_some_and(|c| !c.is_empty()));
        let said = crate::host_text::api_error(body).filter(|_| coded);
        match (status, said) {
            (200..=299, _) => Outcome::Done,
            (_, Some(said)) => Outcome::Refused(said),
            (code, None) => Outcome::Failed(pf_l10n::client::error_mgmt_refused(code)),
        }
    }

    /// The line a player reads after hiding (`hidden`) or showing `title` for profile `name`.
    pub fn notice(&self, hidden: bool, title: &str, name: &str) -> String {
        use pf_l10n::client as m;
        match (self, hidden) {
            (Outcome::Done, true) => m::lib_hidden_done(title, name),
            (Outcome::Done, false) => m::lib_shown_done(title, name),
            (Outcome::Refused(said), _) => said.clone(),
            (Outcome::Failed(why), true) => m::lib_hide_failed(title, why),
            (Outcome::Failed(why), false) => m::lib_show_failed(title, why),
        }
    }
}

/// One title on the hidden list, as the page marks it.
#[derive(serde::Deserialize)]
struct Listed {
    #[serde(flatten)]
    entry: GameEntry,
    #[serde(default)]
    hidden: bool,
}

/// What `profile` hides, its person asking: every page of `hidden=true`, keeping only the rows
/// marked hidden, so a host that ignores the flag lists nothing. `get` answers one path.
pub fn fetch_hidden(
    profile: &str,
    mut get: impl FnMut(&str) -> Result<(u16, Vec<u8>), LibraryError>,
) -> Result<Vec<GameEntry>, LibraryError> {
    let rows: Vec<Listed> = walk_pages(
        |cursor| {
            let mut path = format!("/api/v1/library/page?limit={PAGE_LIMIT}&hidden=true");
            if let Some(c) = cursor {
                path.push_str("&cursor=");
                path.push_str(&query_value(c));
            }
            path.push_str("&as=");
            path.push_str(&query_value(profile));
            let (status, body) = get(&path)?;
            let body = Reply { status, body }.ok()?;
            Ok(String::from_utf8_lossy(&body).into_owned())
        },
        LibraryError::Unreachable,
    )?;
    Ok(rows
        .into_iter()
        .filter(|r| r.hidden)
        .map(|r| r.entry)
        .collect())
}

/// Hide (`hidden`) or show title `id` for `profile` on the host `api` reaches. Blocking.
#[cfg(desktop)]
pub fn set_hidden(api: &crate::library::HostApi, profile: &str, id: &str, hidden: bool) -> Outcome {
    let https = match crate::library::agent(&api.identity, api.pin) {
        Ok(a) => a,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let (path, body) = request(profile, id, hidden);
    let json = Some(("application/json", body.as_bytes()));
    match api.call(&https, "PUT", &path, json) {
        Ok(r) => Outcome::from_reply(r.status, &String::from_utf8_lossy(&r.body)),
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// [`fetch_hidden`] over `api`'s lanes. Blocking.
#[cfg(desktop)]
pub fn fetch_hidden_from(
    api: &crate::library::HostApi,
    profile: &str,
) -> Result<Vec<GameEntry>, LibraryError> {
    let https = crate::library::agent(&api.identity, api.pin)?;
    fetch_hidden(profile, |path| {
        let reply = api.call(&https, "GET", path, None)?;
        Ok((reply.status, reply.body))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::BTreeMap;

    #[derive(Deserialize)]
    struct Vectors {
        offer: Vec<OfferCase>,
        set_hidden: Vec<SetCase>,
        hidden_list: Vec<ListCase>,
    }

    #[derive(Deserialize)]
    struct OfferCase {
        name: String,
        listed: Option<Vec<ListedProfile>>,
        pick: Option<String>,
        offered: Option<String>,
    }

    #[derive(Deserialize)]
    struct SetCase {
        name: String,
        profile: String,
        id: String,
        hidden: bool,
        path: String,
        body: serde_json::Value,
        reply: Answer,
        outcome: String,
        said: Option<String>,
    }

    #[derive(Deserialize)]
    struct Answer {
        status: u16,
        body: serde_json::Value,
    }

    #[derive(Deserialize)]
    struct ListCase {
        name: String,
        profile: String,
        host: BTreeMap<String, Answer>,
        asked: Vec<String>,
        ids: Option<Vec<String>>,
    }

    fn vectors() -> Vectors {
        let raw = include_str!("../../../../clients/shared/hide-for-me-vectors.json");
        serde_json::from_str(raw).expect("vectors parse")
    }

    #[test]
    fn the_offer_follows_the_shared_vectors() {
        for c in vectors().offer {
            let row = offer(c.listed.as_deref(), c.pick.as_deref());
            let name = row.map(|p| p.display_name.clone());
            assert_eq!(name, c.offered, "{}", c.name);
        }
    }

    #[test]
    fn a_hide_follows_the_shared_vectors() {
        for c in vectors().set_hidden {
            let (path, body) = request(&c.profile, &c.id, c.hidden);
            assert_eq!(path, c.path, "{}", c.name);
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body, c.body, "{}", c.name);
            let reply = match &c.reply.body {
                serde_json::Value::Null => String::new(),
                body => body.to_string(),
            };
            let outcome = Outcome::from_reply(c.reply.status, &reply);
            let (kind, said) = match &outcome {
                Outcome::Done => ("done", None),
                Outcome::Refused(s) => ("refused", Some(s.clone())),
                Outcome::Failed(_) => ("failed", None),
            };
            assert_eq!(kind, c.outcome, "{}", c.name);
            assert_eq!(said, c.said, "{}", c.name);
        }
    }

    #[test]
    fn the_hidden_list_follows_the_shared_vectors() {
        for c in vectors().hidden_list {
            let mut asked = Vec::new();
            let shown = fetch_hidden(&c.profile, |path| {
                asked.push(path.to_string());
                Ok(c.host.get(path).map_or((404, Vec::new()), |a| {
                    (a.status, a.body.to_string().into_bytes())
                }))
            });
            assert_eq!(asked, c.asked, "{}: asked", c.name);
            let ids = shown.ok().map(|g| g.into_iter().map(|g| g.id).collect());
            assert_eq!(ids, c.ids, "{}: ids", c.name);
        }
    }

    /// Every outcome has a line, and the host's sentence stands as it came.
    #[test]
    fn each_outcome_reads_as_one_line() {
        let refused = Outcome::Refused("This device can't change this profile.".into());
        assert_eq!(
            refused.notice(true, "Dota 2", "Kid"),
            "This device can't change this profile."
        );
        assert_eq!(
            Outcome::Done.notice(true, "Dota 2", "Kid"),
            "Dota 2 is hidden for Kid"
        );
        assert_eq!(
            Outcome::Done.notice(false, "Dota 2", "Kid"),
            "Dota 2 is back for Kid"
        );
        let failed = Outcome::Failed("no answer".into());
        assert_eq!(
            failed.notice(false, "Dota 2", "Kid"),
            "Couldn't show Dota 2 — no answer"
        );
    }
}
