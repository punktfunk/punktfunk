//! A sentence the host sent by the id of its message, in this app's language.
//!
//! The host names the message beside its English: an API error's `code` and `params`, a seat
//! line's `detail_id`, a launch verdict's `message_id`, a close code's `reject_<token>`. An id
//! this build has no message for, such as a newer host's, reads as the host's own English.
//! The host's screen echo is a reason code this app words itself ([`screen_wish_line`]).
//! `clients/shared/host-text-vectors.json` pins the sentences for the Kotlin and Swift copies.

use punktfunk_core::quic::v2::hello::{RoleReason, ScreenWish};

/// What a host may ask this app to say: its API errors, seat lines, launch verdicts and
/// refusals. Any other message is the app's own.
const IDS: [&str; 4] = ["apierr_", "launch_", "seatline_", "reject_"];

/// The message `id` names, filled from `params` (placeholder name, value), else `english`.
/// A `count` parameter picks the plural form.
pub fn word(id: &str, params: &[(&str, &str)], english: &str) -> String {
    let Some(msg) = IDS
        .iter()
        .any(|p| id.starts_with(p))
        .then(|| pf_l10n::get(id))
        .flatten()
    else {
        return english.to_string();
    };
    let args: Vec<(&str, &dyn std::fmt::Display)> = params
        .iter()
        .map(|(name, value)| (*name, value as &dyn std::fmt::Display))
        .collect();
    let count = params
        .iter()
        .find(|(name, _)| *name == "count")
        .and_then(|(_, value)| value.parse::<u64>().ok());
    match count {
        Some(n) => msg.format_count(n, &args),
        None => msg.format(&args),
    }
}

/// [`word`] for an `ApiError` code, whose message is `apierr_<code>`.
pub fn word_code(code: &str, params: &[(&str, &str)], english: &str) -> String {
    word(&format!("apierr_{code}"), params, english)
}

/// The sentence of an `ApiError` body (`{"error", "code", "params"}`): the host's message in
/// this app's language when it names one this build has, else its English. `None` for a body
/// that is not an `ApiError` or says nothing.
pub fn api_error(body: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let english = json["error"].as_str().unwrap_or_default();
    let code = json["code"].as_str().filter(|c| !c.is_empty());
    if english.is_empty() && code.is_none() {
        return None;
    }
    let params: Vec<(&str, &str)> = json["params"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.as_str(), v.as_str()?)))
                .collect()
        })
        .unwrap_or_default();
    let said = match code {
        Some(code) => word_code(code, &params, english),
        None => english.to_string(),
    };
    (!said.is_empty()).then_some(said)
}

/// The stream-start line for a screen the session did not get as asked: `reason` from the
/// host's echo, `wish` the preset's. The held monitors need no wish to name; any other reason
/// names the wished role, or a monitor wish's connector. `None` when there is nothing to say.
pub fn screen_wish_line(wish: Option<&ScreenWish>, reason: RoleReason) -> Option<String> {
    use pf_l10n::client as m;
    let role = || match wish? {
        ScreenWish::Only => Some(m::screen_role_only().to_string()),
        ScreenWish::Main => Some(m::screen_role_main().to_string()),
        ScreenWish::Second => Some(m::screen_role_second().to_string()),
        ScreenWish::Monitor(c) => Some(c.clone()),
    };
    match reason {
        RoleReason::AsAsked => None,
        RoleReason::MonitorsHeld => Some(m::screen_wish_monitors_held().into()),
        RoleReason::NotAllowed => role().map(m::screen_wish_not_allowed),
        RoleReason::Unavailable => role().map(m::screen_wish_unavailable),
        RoleReason::NoMonitors => role().map(m::screen_wish_no_monitors),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case of the shared file words as its `sentence`, and an id this build does not
    /// know, or one that is not the host's to ask for, as the host's `english`.
    #[test]
    fn shared_vectors() {
        let raw = include_str!("../../../../clients/shared/host-text-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        for case in file["cases"].as_array().expect("cases array") {
            let params: Vec<(&str, &str)> = case["params"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str().unwrap()))
                .collect();
            let said = word(
                case["id"].as_str().unwrap(),
                &params,
                case["english"].as_str().unwrap(),
            );
            assert_eq!(said, case["sentence"].as_str().unwrap(), "{case}");
        }
    }

    /// Every `screen_wish` row of the shared file: the preset's spelling and the echo's reason
    /// word as its `sentence`, or as nothing.
    #[test]
    fn screen_wish_vectors() {
        let raw = include_str!("../../../../clients/shared/host-text-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).unwrap();
        let rows = file["screen_wish"].as_array().expect("screen_wish rows");
        assert!(rows.len() >= 4);
        for row in rows {
            let wish = row["wish"].as_str().and_then(ScreenWish::parse);
            let reason: RoleReason = serde_json::from_value(row["reason"].clone()).unwrap();
            let said = screen_wish_line(wish.as_ref(), reason);
            assert_eq!(said.as_deref(), row["sentence"].as_str(), "{row}");
            if let Some(id) = row["id"].as_str() {
                assert!(pf_l10n::get(id).is_some(), "{id}");
            }
        }
    }

    /// A message with a placeholder is in the file, so a client that binds it by position
    /// gets it checked. The file lists the keys the clients word.
    #[test]
    fn the_vectors_cover_every_message_with_a_placeholder() {
        let messages: serde_json::Value =
            serde_json::from_str(include_str!("../../../../l10n/messages/host/en.json")).unwrap();
        let raw = include_str!("../../../../clients/shared/host-text-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).unwrap();
        let ids: Vec<&str> = file["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        for (key, text) in messages.as_object().unwrap() {
            if !IDS.iter().any(|p| key.starts_with(p)) {
                continue;
            }
            let has_placeholder = text.is_array() || text.as_str().unwrap().contains('{');
            let is_refusal = key.starts_with("reject_");
            assert!(
                !(has_placeholder || is_refusal) || ids.contains(&key.as_str()),
                "{key} is not in host-text-vectors.json"
            );
        }
    }

    #[test]
    fn an_api_error_is_worded_by_its_code() {
        let body = r#"{"error":"That isn't jdoe's password.","code":"credential_wrong",
            "params":{"account":"jdoe"}}"#;
        assert_eq!(
            api_error(body).as_deref(),
            Some("That isn't jdoe's password.")
        );
        pf_l10n::with_language("en-XA", || {
            let said = api_error(body).unwrap();
            assert!(said.starts_with('[') && said.contains("jdoe"), "{said}");
        });
        // A code from a newer host, or none at all: the host's English.
        let newer = r#"{"error":"Quit Quail first.","code":"from_a_newer_host"}"#;
        assert_eq!(api_error(newer).as_deref(), Some("Quit Quail first."));
        assert_eq!(
            api_error(r#"{"error":"unknown setting id"}"#).as_deref(),
            Some("unknown setting id")
        );
        assert_eq!(api_error("<html>"), None);
        assert_eq!(api_error(r#"{"error":""}"#), None);
    }
}
