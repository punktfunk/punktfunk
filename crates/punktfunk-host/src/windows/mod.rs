//! The Windows-only halves of the host: the SCM service, the per-user tray, the installer
//! work (`driver`, `web`), the interactive-session spawn, and the process entry hooks
//! ([`entry`]). `main.rs` re-imports the flat modules so `crate::install::*`
//! keeps its historic path; `entry` is the one seam `main` calls through.

// The Windows-only devtests; the cross-platform ones stay in `crate::devtest`.
pub(crate) mod devtest;
pub(crate) mod entry;
// The IDD-push manager's session touch points; every other OS gets the no-op twin in `main.rs`.
pub(crate) mod idd;
// Pause NVIDIA Instant Replay while it shares the encoder with a stream; resume after.
pub(crate) mod instant_replay;
// WM_CLOSE on the interactive desktop, then TerminateProcess.
pub(crate) mod capture_worker;
pub(crate) mod game_term;
pub(crate) mod install;
pub(crate) mod interactive;
pub(crate) mod plugin_pipe;
pub(crate) mod registry_ace;
pub(crate) mod service;
// The console user's theme values, read for `mgmt::theme` — which forbids the `unsafe` they need.
pub(crate) mod theme;
// Per-user tray start/stop/status — the only recovery path after a crash or upgrade.
pub(crate) mod tray;

/// A secret the host did not write is not a credential. Only Windows can be pre-planted:
/// `%ProgramData%` grants Users create, while the Unix config dir is 0700 from birth.
pub(crate) mod planted {
    pub(crate) use pf_paths_win::quarantine_planted_secret;
}
