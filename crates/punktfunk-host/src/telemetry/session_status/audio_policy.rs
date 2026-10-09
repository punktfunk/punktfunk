//! A title's `audio.sessions` policy: which live sessions hear it, applied through the
//! operator's own per-session mute.

use std::sync::{Mutex, PoisonError};

use super::{registry, LiveSession};

/// Which sessions hear a title's audio (`audio.sessions` on a custom entry).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum AudioSessions {
    #[default]
    All,
    /// The session that owns the display; joiners are silent.
    Owner,
    /// The sessions that joined the display; the owner is silent.
    Joined,
    /// Only the session that launched the title.
    Launcher,
}

/// Whether `policy` silences a session. `launcher` is the launching session's
/// client label, matched the way [`super::stop_by_fingerprint`] matches.
fn policy_mutes(policy: AudioSessions, launcher: &str, client: &str, join: bool) -> bool {
    match policy {
        AudioSessions::All => false,
        AudioSessions::Owner => join,
        AudioSessions::Joined => !join,
        AudioSessions::Launcher => client != launcher,
    }
}

struct AudioPolicy {
    sessions: AudioSessions,
    launcher: String,
    /// Sessions this policy muted, so its end unmutes exactly those.
    muted: Vec<u64>,
}

/// The live title policy, applied to sessions that register while it stands.
/// One at a time: a second lease replaces it, and the first to end lifts both.
static AUDIO_POLICY: Mutex<Option<AudioPolicy>> = Mutex::new(None);

/// Apply a title's `audio.sessions` through the same mute the operator uses, so
/// the client is told and the operator can unmute live. Drop unmutes what it muted.
pub fn apply_audio_policy(sessions: AudioSessions, launcher: &str) -> AudioPolicyGuard {
    let mut muted = Vec::new();
    for s in registry().iter() {
        // Compat sessions have no per-session mute, and marking one muted without muting it
        // would put a Muted badge on a session the operator can still hear.
        if s.plane != crate::events::Plane::Gamestream
            && policy_mutes(sessions, launcher, &s.client, s.join)
        {
            s.controls.set_muted(true);
            muted.push(s.id);
        }
    }
    let mut live = AUDIO_POLICY.lock().unwrap_or_else(PoisonError::into_inner);
    // Replacing a live policy inherits what it muted: the first lease to end lifts both.
    if let Some(prev) = live.take() {
        muted.extend(prev.muted);
    }
    *live = Some(AudioPolicy {
        sessions,
        launcher: launcher.to_owned(),
        muted,
    });
    drop(live);
    tracing::info!(policy = ?sessions, launcher, "title audio policy applied");
    AudioPolicyGuard(())
}

/// A standing title policy reaches a session that arrives under it.
pub(super) fn apply_to_new(s: &LiveSession) {
    if let Some(p) = AUDIO_POLICY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
    {
        if s.plane != crate::events::Plane::Gamestream
            && policy_mutes(p.sessions, &p.launcher, &s.client, s.join)
        {
            s.controls.set_muted(true);
            p.muted.push(s.id);
        }
    }
}

pub struct AudioPolicyGuard(());

impl Drop for AudioPolicyGuard {
    fn drop(&mut self) {
        let Some(policy) = AUDIO_POLICY
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return;
        };
        for s in registry().iter() {
            if policy.muted.contains(&s.id) {
                s.controls.set_muted(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::session_status::tests::{fake_joiner, registry_lock};

    #[test]
    fn policy_picks_the_sessions_it_names() {
        use AudioSessions::*;
        // (policy, client, join) → muted
        for (policy, client, join, want) in [
            (All, "aaaaaaaaaaaa", true, false),
            (Owner, "aaaaaaaaaaaa", false, false),
            (Owner, "bbbbbbbbbbbb", true, true),
            (Joined, "aaaaaaaaaaaa", false, true),
            (Joined, "bbbbbbbbbbbb", true, false),
            (Launcher, "aaaaaaaaaaaa", false, false),
            (Launcher, "bbbbbbbbbbbb", true, true),
        ] {
            assert_eq!(
                policy_mutes(policy, "aaaaaaaaaaaa", client, join),
                want,
                "{policy:?} {client} join={join}"
            );
        }
    }

    /// A policy mutes by role, reaches a joiner that registers later, and its end
    /// unmutes only what it muted: an operator's own mute stays.
    #[test]
    fn policy_mute_reaches_late_joiners_and_lifts_at_the_end() {
        let _registry = registry_lock();
        let (_owner, owner) = fake_joiner("cccccccccccc", false);
        let guard = apply_audio_policy(AudioSessions::Owner, "cccccccccccc");
        let (_joiner, joiner) = fake_joiner("dddddddddddd", true);
        assert!(!owner.muted.load(Ordering::SeqCst));
        assert!(
            joiner.muted.load(Ordering::SeqCst),
            "a joiner registered under the policy is muted"
        );
        owner.set_muted(true);
        drop(guard);
        assert!(
            !joiner.muted.load(Ordering::SeqCst),
            "the lease ending lifts the policy mute"
        );
        assert!(
            owner.muted.load(Ordering::SeqCst),
            "the operator's mute outlives the policy"
        );
    }

    /// A second lease replaces the first; the first to end lifts both, so nothing the
    /// replaced policy muted stays muted with no policy left to lift it.
    #[test]
    fn a_replaced_policy_mute_still_lifts() {
        let _registry = registry_lock();
        let (_owner, _o) = fake_joiner("eeeeeeeeeeee", false);
        let (_joiner, joiner) = fake_joiner("ffffffffffff", true);
        let first = apply_audio_policy(AudioSessions::Launcher, "eeeeeeeeeeee");
        assert!(joiner.muted.load(Ordering::SeqCst));
        let second = apply_audio_policy(AudioSessions::All, "eeeeeeeeeeee");
        drop(first);
        assert!(
            !joiner.muted.load(Ordering::SeqCst),
            "the first lease ending lifts what either policy muted"
        );
        drop(second);
    }
}
