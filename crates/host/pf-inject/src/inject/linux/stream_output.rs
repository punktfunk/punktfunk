//! Named compositor output for absolute input — Linux counterpart of the Windows
//! `stream_target` slot. The wlroots virtual pointer maps `MouseMoveAbs` onto the
//! `wl_output` it was **created with**; `w`/`h` on the event is the client's
//! letterboxed video rect, not the streamed mode, so the extent is already in the
//! protocol and the OUTPUT is the only pin.
//!
//! The host publishes [`set_stream_output`] at capture bring-up (`wl_output.name`,
//! protocol v4, "the same for all clients": Hyprland `PF-<pid>-<n>`, sway
//! `HEADLESS-N`, KWin `Virtual-<name>`, or a mirrored connector). The wlr backend
//! re-creates the pointer bound to that name; KWin fake_input maps into that head.
//!
//! **One slot per process.** Last capture bring-up wins for every concurrent
//! session; per-session routing needs source-tagged events (see
//! [`crate::set_absolute_anchor`]). Unpublished, or a compositor without v4 names,
//! binds no output and maps over the whole layout — reachable, unlike a pin to the
//! first-advertised (oldest, operator) head.

use std::sync::RwLock;

static STREAM_OUTPUT: RwLock<Option<String>> = RwLock::new(None);

/// Host-only, at capture bring-up. Never cleared on teardown: a vanished output
/// stops resolving and the backend maps the whole layout; nothing injects between
/// sessions. A later bring-up may pass `None` so a stale name cannot outlive its
/// compositor. Parallel sessions: last writer wins (module doc).
pub fn set_stream_output(name: Option<String>) {
    crate::bump_aim_gen();
    let mut cur = STREAM_OUTPUT.write().unwrap_or_else(|e| e.into_inner());
    if *cur != name {
        tracing::info!(output = ?name, "absolute-input stream output set");
        *cur = name;
    }
}

pub fn stream_output() -> Option<String> {
    STREAM_OUTPUT
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test: the slot is process-wide and cargo runs tests on threads in one
    /// process, so splitting this would race.
    #[test]
    fn publishes_clears_and_round_trips() {
        set_stream_output(Some("PF-1643-1".into()));
        assert_eq!(stream_output().as_deref(), Some("PF-1643-1"));
        // Same name is a no-op: the backend keys pointer re-creation off the
        // resolved name; the log line must not repeat.
        set_stream_output(Some("PF-1643-1".into()));
        assert_eq!(stream_output().as_deref(), Some("PF-1643-1"));
        set_stream_output(Some("HEADLESS-2".into()));
        assert_eq!(stream_output().as_deref(), Some("HEADLESS-2"));
        set_stream_output(None);
        assert_eq!(stream_output(), None);
    }
}
