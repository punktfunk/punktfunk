//! A launched game beside the session that asked for it: the launch sequence, the lease that
//! ties the two together, and how the host tells the game still runs
//! (`design/session-game-lifetime.md`).

// Session⇄game lease: when the game started, when it exits, how to end it.
pub(crate) mod gamelease;
// Launch holds: plugins and hooks that act before a game starts.
pub(crate) mod holds;
// Re-`Hello::launch` must not start a second copy.
pub(crate) mod launchreg;
// Process-table half of session⇄game binding. Empty on macOS.
pub(crate) mod procscan;
// Plugin-reported liveness; `procscan` only sees the process table.
pub(crate) mod runstate;
pub(crate) mod session_launch;
// Operator policy for session⇄game binding (`session-settings.json`).
pub(crate) mod session_settings;
pub(crate) mod stream_marker;
