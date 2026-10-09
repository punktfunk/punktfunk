//! One pass over the PipeWire registry for the apps playing audio right now.
//!
//! Feeds the console's voice-chat app picker, so it names apps the way
//! `pf_host_config::voice_app_matches` matches them: process binary first, then
//! `application.name`, lowercased.

use anyhow::Result;

/// Every audio output stream's app, deduped and sorted. The host's own streams are left out.
pub fn playing_apps() -> Result<Vec<String>> {
    use pipewire as pw;
    use std::cell::RefCell;
    use std::rc::Rc;

    // The console's request thread waits on this; the one-shot's timeout bounds a stalled daemon.
    let session = super::pw_oneshot::OneShot::connect("playing-apps", super::pw_oneshot::TIMEOUT)?;
    let apps: Rc<RefCell<Vec<String>>> = Rc::default();
    let _registry_listener = session
        .registry
        .add_listener_local()
        .global({
            let apps = apps.clone();
            move |g| {
                let Some(props) = g.props else { return };
                if !matches!(g.type_, pw::types::ObjectType::Node)
                    || props.get("media.class") != Some("Stream/Output/Audio")
                {
                    return;
                }
                let name = app_name(
                    props.get("application.process.binary"),
                    props.get("application.name"),
                );
                let mut apps = apps.borrow_mut();
                if let Some(name) = name.filter(|n| !apps.contains(n)) {
                    apps.push(name);
                }
            }
        })
        .register();
    // One round: the registry replays every global before the `done` for this seq.
    session.round()?;

    let mut out = apps.take();
    out.sort();
    Ok(out)
}

fn app_name(binary: Option<&str>, name: Option<&str>) -> Option<String> {
    [binary, name]
        .into_iter()
        .flatten()
        .map(|s| s.trim().to_ascii_lowercase())
        .find(|s| !s.is_empty() && !s.contains(','))
        .filter(|s| s != "punktfunk-host")
}

#[cfg(test)]
mod tests {
    use super::app_name;

    #[test]
    fn names_prefer_the_binary_and_skip_the_host() {
        assert_eq!(
            app_name(Some("Discord"), Some("WEBRTC VoiceEngine")).as_deref(),
            Some("discord")
        );
        assert_eq!(
            app_name(None, Some(" Firefox ")).as_deref(),
            Some("firefox")
        );
        assert_eq!(app_name(Some(""), None), None);
        // A comma would split into two entries in the env form of the list.
        assert_eq!(app_name(Some("a,b"), None), None);
        assert_eq!(app_name(Some("punktfunk-host"), None), None);
    }
}
