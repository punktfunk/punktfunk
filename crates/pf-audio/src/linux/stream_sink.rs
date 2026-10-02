//! Session-scoped default claims for the host's own audio nodes.
//!
//! In stream-sink mode (see [`super`]) the capture stream is an `Audio/Sink`
//! node and must be the session default so host apps play into it; the shared
//! virtual mic is the default source for a session's span, so an app that
//! records the default hears the client. A claim saves the configured default
//! and points it at our node; release restores. WirePlumber's
//! `linking.follow-default-target` then moves running streams. A claimed sink
//! does not depend on display hardware, so a modeset that drops HDMI cannot
//! flip the default under capture.
//!
//! Refcounted, latest-wins: concurrent sessions each hold a claim; the newest
//! routes to *its* node, the newest leaving hands the default to the next
//! newest, and only the last release restores. A `join` session claims the
//! sink it taps, so the owner leaving first restores nothing under it. The
//! ledger lock is held across the metadata round-trip so a stale restore
//! cannot overwrite a fresh claim.
//!
//! Crash self-healing: the pre-claim default is also kept on disk. A leftover
//! name of ours is never a restore target; host start writes the disk copy back.

use anyhow::Result;
use std::sync::Mutex;

/// `node.name` prefix for every host-owned stream sink. Full names uniqued
/// per capturer (`punktfunk-speaker-<pid>-<seq>`) so overlapping capturers
/// never alias. The staleness rule matches this prefix.
pub(super) const SINK_NAME_PREFIX: &str = "punktfunk-speaker";

/// `node.name` of the shared virtual mic, and the prefix of every isolated one.
pub(super) const MIC_NAME: &str = "punktfunk-mic";

/// One WirePlumber default the host claims. Keys sit on subject 0 of `default` metadata; values
/// are `{"name":"<node.name>"}` typed `Spa:String:JSON`.
pub(super) struct DefaultClaim {
    /// The operator's preferred node, which a claim replaces.
    configured_key: &'static str,
    /// WirePlumber's elected node: what the operator hears when nothing is configured.
    effective_key: &'static str,
    /// Name prefix that marks a node as ours.
    ours: &'static str,
    /// Under [`pf_paths::state_dir`]: the pre-claim value, for the host start after a crash.
    saved_file: &'static str,
    ledger: Mutex<Ledger>,
}

pub(super) static SINK: DefaultClaim = DefaultClaim {
    configured_key: "default.configured.audio.sink",
    effective_key: "default.audio.sink",
    ours: SINK_NAME_PREFIX,
    saved_file: "default-audio-sink.json",
    ledger: Mutex::new(Ledger::new()),
};

pub(super) static SOURCE: DefaultClaim = DefaultClaim {
    configured_key: "default.configured.audio.source",
    effective_key: "default.audio.source",
    ours: MIC_NAME,
    saved_file: "default-audio-source.json",
    ledger: Mutex::new(Ledger::new()),
};

#[derive(Debug, PartialEq)]
enum Restore {
    /// Re-set the saved pre-claim JSON (`{"name":"..."}`).
    Value(String),
    /// No pre-claim preference, or the saved one was a stale punktfunk claim.
    Delete,
}

/// What a release writes: the newest remaining claim's node, or the pre-claim default.
#[derive(Debug, PartialEq)]
enum Release {
    Repoint(String),
    Restore(Restore),
}

/// Claim bookkeeping, split from PipeWire I/O so the restore rules unit-test
/// on every platform.
struct Ledger {
    /// Held claims' node names, oldest first; the last one is the configured default.
    claims: Vec<String>,
    restore: Option<Restore>,
    /// `node.name` the operator heard on before the first claim: the host
    /// bridge's playthrough and voice-chat target. Kept across releases.
    host: Option<String>,
}

impl Ledger {
    const fn new() -> Ledger {
        Ledger {
            claims: Vec::new(),
            restore: None,
            host: None,
        }
    }

    /// Remember the elected node the first claim found. A stale punktfunk
    /// name (crash leftover) is not an output; the last real one stands.
    fn note_host(&mut self, effective: Option<&str>, ours: &str) {
        if let Some(name) = effective.and_then(json_name).filter(|n| !n.contains(ours)) {
            self.host = Some(name.to_owned());
        }
    }

    /// Count a new claim. `true` = first holder; caller must [`note_previous`].
    fn on_claim(&mut self, node: &str) -> bool {
        self.claims.push(node.to_owned());
        self.claims.len() == 1
    }

    /// Apply the staleness rule to what the first claim found. A leftover claim of ours stands
    /// for `saved`, the value a crashed host kept from before its own claim.
    fn note_previous(&mut self, prev: Option<String>, ours: &str, saved: Option<String>) {
        self.restore = Some(match prev {
            Some(v) if !v.contains(ours) => Restore::Value(v),
            Some(_) => saved.map_or(Restore::Delete, Restore::Value),
            None => Restore::Delete,
        });
    }

    /// Count a release. The newest leaving re-points to the next newest; the last holder
    /// returns the restore action. An older holder leaving writes nothing.
    fn on_release(&mut self, node: &str) -> Option<Release> {
        let i = self.claims.iter().rposition(|c| c == node)?;
        let newest = i + 1 == self.claims.len();
        self.claims.remove(i);
        match self.claims.last() {
            None => self.restore.take().map(Release::Restore),
            Some(top) if newest && top != node => Some(Release::Repoint(top.clone())),
            Some(_) => None,
        }
    }
}

/// The `name` inside WirePlumber's `{"name":"…"}` value.
fn json_name(value: &str) -> Option<&str> {
    let rest = value.split_once(r#""name":""#)?.1;
    rest.split_once('"').map(|(name, _)| name)
}

impl DefaultClaim {
    /// Point the configured default at `node` (refcounted; see module docs). Never fails the
    /// caller: missing WirePlumber still captures; apps just are not rerouted.
    pub(super) fn claim(&self, node: &str) {
        let mut ledger = self.ledger.lock().unwrap();
        let first = ledger.on_claim(node);
        // Latest claim wins: even with an existing holder, route to the newest session's node.
        match self.write(Some(&format!(r#"{{"name":"{node}"}}"#))) {
            Ok(seen) => {
                if first {
                    ledger.note_previous(seen.configured, self.ours, self.read_saved());
                    ledger.note_host(seen.effective.as_deref(), self.ours);
                    self.save(ledger.restore.as_ref());
                }
                tracing::info!(
                    node,
                    key = self.configured_key,
                    "claimed the audio default for the stream session"
                );
            }
            Err(e) => {
                if first {
                    // Nothing to restore: release deletes the key only if a later claim
                    // landed ours there, and leaves an operator's own pick alone.
                    ledger.note_previous(None, self.ours, None);
                }
                tracing::warn!(error = %format!("{e:#}"), key = self.configured_key,
                    "audio default not claimed — host apps may keep the previous device");
            }
        }
    }

    /// Release `node`'s claim. The newest leaving hands the default to the next newest
    /// session's node; the last release writes the pre-claim default back.
    pub(super) fn release(&self, node: &str) {
        let mut ledger = self.ledger.lock().unwrap();
        match ledger.on_release(node) {
            None => {}
            Some(Release::Restore(r)) => self.apply(r),
            Some(Release::Repoint(top)) => {
                match self.write(Some(&format!(r#"{{"name":"{top}"}}"#))) {
                    Ok(_) => tracing::info!(
                        node = top,
                        key = self.configured_key,
                        "audio default back on the remaining stream session"
                    ),
                    Err(e) => tracing::warn!(error = %format!("{e:#}"), key = self.configured_key,
                        "audio default not handed back — host apps may keep the ended session's device"),
                }
            }
        }
    }

    /// Host stopping: drop every claim and write the pre-claim default back. The process exits
    /// without running the destructors that would.
    pub(super) fn release_all(&self) {
        let mut ledger = self.ledger.lock().unwrap();
        if ledger.claims.is_empty() {
            return;
        }
        ledger.claims.clear();
        if let Some(restore) = ledger.restore.take() {
            self.apply(restore);
        }
    }

    /// Host start: a default still naming a node of ours is a crashed host's claim. Write the
    /// saved pre-claim value back. PipeWire not up keeps the saved copy for a later start.
    pub(super) fn heal(&self) {
        let ledger = self.ledger.lock().unwrap();
        if !ledger.claims.is_empty() {
            return;
        }
        let Ok(current) = self.read() else { return };
        if current.as_deref().is_some_and(|v| v.contains(self.ours)) {
            self.apply(self.read_saved().map_or(Restore::Delete, Restore::Value));
        } else {
            let _ = std::fs::remove_file(self.saved_path());
        }
    }

    fn apply(&self, restore: Restore) {
        let value = match &restore {
            Restore::Value(v) => Some(v.as_str()),
            // Delete only our own claim. A key naming no node of ours is the operator's pick
            // since, or a claim that never landed: deleting it would lose their choice.
            Restore::Delete
                if self
                    .read()
                    .is_ok_and(|c| !c.as_deref().is_some_and(|v| v.contains(self.ours))) =>
            {
                let _ = std::fs::remove_file(self.saved_path());
                return;
            }
            Restore::Delete => None,
        };
        match self.write(value) {
            Ok(_) => {
                let _ = std::fs::remove_file(self.saved_path());
                tracing::info!(
                    restored = value.unwrap_or("<automatic>"),
                    key = self.configured_key,
                    "restored the audio default after the stream session"
                );
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), key = self.configured_key,
                "audio default not restored — set it manually (wpctl set-default)"),
        }
    }

    fn saved_path(&self) -> std::path::PathBuf {
        pf_paths::state_dir().join(self.saved_file)
    }

    fn read_saved(&self) -> Option<String> {
        std::fs::read_to_string(self.saved_path())
            .ok()
            .filter(|v| !v.is_empty() && !v.contains(self.ours))
    }

    /// A `Delete` needs no copy: after a crash, no saved value already means "automatic".
    fn save(&self, restore: Option<&Restore>) {
        let path = self.saved_path();
        match restore {
            Some(Restore::Value(v)) => {
                let _ = pf_paths::create_private_dir(&pf_paths::state_dir());
                let _ = std::fs::write(&path, v);
            }
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Read both keys from `default` metadata, then set `value` on the configured key (`None`
    /// deletes). Own short-lived main loop on the calling thread — claims come from session
    /// start/end, never a PW callback. The one-shot's timeout bounds a sick-but-connected daemon.
    fn write(&self, value: Option<&str>) -> Result<Seen> {
        let session = super::pw_oneshot::OneShot::connect("claim", super::pw_oneshot::TIMEOUT)?;
        let (metadata, mut defaults) = session.default_metadata()?;
        metadata.set_property(
            0,
            self.configured_key,
            value.map(|_| "Spa:String:JSON"),
            value,
        );
        session.round()?; // flush the write before the proxies drop
        Ok(Seen {
            configured: defaults.remove(self.configured_key),
            effective: defaults.remove(self.effective_key),
        })
    }

    fn read(&self) -> Result<Option<String>> {
        let session = super::pw_oneshot::OneShot::connect("claim", super::pw_oneshot::TIMEOUT)?;
        let (_metadata, mut defaults) = session.default_metadata()?;
        Ok(defaults.remove(self.configured_key))
    }
}

/// What the metadata held before a write: the configured value this claim
/// replaces, and the elected node the operator was hearing on.
struct Seen {
    configured: Option<String>,
    effective: Option<String>,
}

/// The output the operator heard before the stream claimed the default, if a
/// claim has seen one. `None` until then, or with no session manager.
pub(super) fn host_sink() -> Option<String> {
    SINK.ledger.lock().unwrap().host.clone()
}

/// Point the configured default sink at `sink_name`.
pub(super) fn claim(sink_name: &str) {
    SINK.claim(sink_name);
}

/// Release `sink_name`'s default-sink claim.
pub(super) fn release(sink_name: &str) {
    SINK.release(sink_name);
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str = SINK_NAME_PREFIX;

    #[test]
    fn claim_release_roundtrip() {
        let mut l = Ledger::new();
        assert!(l.on_claim("a"), "first claim must save the previous value");
        l.note_previous(Some(r#"{"name":"alsa_output.hdmi"}"#.into()), OURS, None);
        assert_eq!(
            l.on_release("a"),
            Some(Release::Restore(Restore::Value(
                r#"{"name":"alsa_output.hdmi"}"#.into()
            )))
        );
    }

    #[test]
    fn nested_claims_restore_once() {
        let mut l = Ledger::new();
        assert!(l.on_claim("a"));
        l.note_previous(Some(r#"{"name":"alsa_output.hdmi"}"#.into()), OURS, None);
        assert!(
            !l.on_claim("a"),
            "second claim must not overwrite the saved value"
        );
        assert_eq!(l.on_release("a"), None, "inner release must not restore");
        assert_eq!(
            l.on_release("a"),
            Some(Release::Restore(Restore::Value(
                r#"{"name":"alsa_output.hdmi"}"#.into()
            )))
        );
    }

    /// The newest session leaving hands the default to the one still streaming; an older one
    /// leaving writes nothing, since the default is not its sink.
    #[test]
    fn the_newest_leaving_repoints_to_the_next_newest() {
        let mut l = Ledger::new();
        assert!(l.on_claim("a"));
        l.note_previous(None, OURS, None);
        assert!(!l.on_claim("b"));
        assert_eq!(l.on_release("b"), Some(Release::Repoint("a".into())));
        assert!(!l.on_claim("c"));
        assert_eq!(l.on_release("a"), None, "c still holds the default");
        assert_eq!(l.on_release("c"), Some(Release::Restore(Restore::Delete)));
    }

    /// A leftover `punktfunk-speaker-*` name must not become the restore target.
    #[test]
    fn stale_own_claim_degrades_to_delete() {
        let mut l = Ledger::new();
        assert!(l.on_claim("a"));
        l.note_previous(
            Some(r#"{"name":"punktfunk-speaker-4242-0"}"#.into()),
            OURS,
            None,
        );
        assert_eq!(l.on_release("a"), Some(Release::Restore(Restore::Delete)));
    }

    /// After a crash the leftover claim stands for what the crashed host saved: the operator's
    /// own pick comes back instead of being erased.
    #[test]
    fn stale_own_claim_restores_the_saved_pick() {
        let mut l = Ledger::new();
        assert!(l.on_claim("a"));
        let saved = r#"{"name":"alsa_output.usb"}"#.to_string();
        l.note_previous(
            Some(r#"{"name":"punktfunk-speaker-4242-0"}"#.into()),
            OURS,
            Some(saved.clone()),
        );
        assert_eq!(
            l.on_release("a"),
            Some(Release::Restore(Restore::Value(saved)))
        );
    }

    #[test]
    fn unset_previous_deletes() {
        let mut l = Ledger::new();
        assert!(l.on_claim("a"));
        l.note_previous(None, OURS, None);
        assert_eq!(l.on_release("a"), Some(Release::Restore(Restore::Delete)));
    }

    /// The host output is the elected sink's bare name; a stale punktfunk
    /// election or a missing key keeps the last real one.
    #[test]
    fn host_output_is_the_elected_sink_name() {
        assert_eq!(
            json_name(r#"{"name":"alsa_output.hdmi"}"#),
            Some("alsa_output.hdmi")
        );
        assert_eq!(json_name("garbage"), None);
        let mut l = Ledger::new();
        l.note_host(Some(r#"{"name":"alsa_output.hdmi"}"#), OURS);
        assert_eq!(l.host.as_deref(), Some("alsa_output.hdmi"));
        l.note_host(Some(r#"{"name":"punktfunk-speaker-4242-0"}"#), OURS);
        assert_eq!(l.host.as_deref(), Some("alsa_output.hdmi"));
        l.note_host(None, OURS);
        assert_eq!(l.host.as_deref(), Some("alsa_output.hdmi"));
    }

    /// Unbalanced release must not underflow or restore.
    #[test]
    fn unbalanced_release_is_harmless() {
        let mut l = Ledger::new();
        assert_eq!(l.on_release("a"), None);
        assert!(
            l.on_claim("a"),
            "ledger must stay usable after an unbalanced release"
        );
    }
}
