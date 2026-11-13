//! What the host made of this session's library launch, for the launch hold of either shell.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::jlong;
use jni::EnvUnowned;
use punktfunk_core::quic::{LaunchOutcome, LaunchOutcomeKind};

use super::SESSIONS;

/// `NativeBridge.nativeLaunchRefusal(handle): String` — the host's sentence when it did not start
/// this session's game (refused it, or it closed again at once), as
/// `{"id", "message", "params": {name: value}}`; `""` otherwise. The latest verdict wins, so poll
/// it. Cheap; safe on the UI thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeLaunchRefusal<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    let out = SESSIONS
        .get(handle)
        .and_then(|session| session.client.launch_outcome())
        .and_then(|o| refusal_json(&o))
        .unwrap_or_default();
    env.with_env(|env| env.new_string(out))
        .resolve::<LogErrorAndDefault>()
}

/// `o` as `nativeLaunchRefusal` hands it over, when the host started nothing and said why.
fn refusal_json(o: &LaunchOutcome) -> Option<String> {
    let started_nothing = matches!(
        o.kind,
        LaunchOutcomeKind::Refused | LaunchOutcomeKind::Failed
    );
    if !started_nothing || o.notice().is_none() {
        return None;
    }
    let params: serde_json::Map<String, serde_json::Value> = o
        .message_params
        .iter()
        .map(|(name, value)| (name.clone(), value.clone().into()))
        .collect();
    Some(
        serde_json::json!({ "id": o.message_id, "message": o.message, "params": params })
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::refusal_json;
    use punktfunk_core::quic::{LaunchOutcome, LaunchOutcomeKind as Kind};

    fn parsed(o: &LaunchOutcome) -> serde_json::Value {
        serde_json::from_str(&refusal_json(o).expect("a refusal")).unwrap()
    }

    /// A launch the host did not start reaches Kotlin as its message, English and values; one
    /// that gave the player a game, or that the host said nothing about, reaches it as nothing.
    #[test]
    fn a_refused_launch_carries_the_hosts_sentence() {
        let hidden = "Couldn't start that game \u{2014} it isn't in this host's library anymore.";
        let refused =
            LaunchOutcome::new(Kind::Refused, hidden).worded("launch_title_not_in_library", &[]);
        assert_eq!(
            parsed(&refused),
            serde_json::json!({ "id": "launch_title_not_in_library", "message": hidden, "params": {} })
        );
        let closed = LaunchOutcome::new(
            Kind::Failed,
            "Couldn't start Quail \u{2014} it closed again after 4 s.",
        )
        .worded(
            "launch_closed_at_once",
            &[("title", "Quail"), ("seconds", "4")],
        );
        assert_eq!(
            parsed(&closed)["params"],
            serde_json::json!({ "title": "Quail", "seconds": "4" })
        );
        for kind in [
            Kind::Spawned,
            Kind::Adopted,
            Kind::AdoptedUnknown,
            Kind::SignInNeeded,
        ] {
            let said = LaunchOutcome::new(kind, "Quail was started a moment ago.");
            assert_eq!(refusal_json(&said), None, "{kind:?} is no refusal");
        }
        assert_eq!(
            refusal_json(&LaunchOutcome::new(Kind::Refused, "")),
            None,
            "a host that says nothing leaves the hold to its own clock"
        );
    }
}
