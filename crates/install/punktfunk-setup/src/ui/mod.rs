//! Everything that renders. Nothing below this module knows which surface is attached.
//!
//! `exec` and `report` speak only `Reporter`, so the same `Plan` drives the
//! `==>` transcript and the TUI checklist. `plain` is the D5 compatibility
//! contract — byte-for-byte the sh installer's output. `tui` is the clack
//! screens, hand-rolled over `term` (see `term`'s header for why).
//!
//! `summary` is the settings screen as pure state, `theme` the brand colour and
//! capability ladder, `logo` the animated lens mark.

pub mod logo;
pub mod plain;
pub mod summary;
pub mod term;
pub mod theme;
pub mod tui;

pub use plain::Plain;

/// The surface a running plan reports itself through.
///
/// Vocabulary matches the sh installer: a step heading, an ok, a warning, an
/// echoed command. A checklist renderer builds from `say` and `plus`.
pub trait Reporter {
    fn say(&self, msg: &str);
    fn ok(&self, msg: &str);
    fn warn(&self, msg: &str);
    fn die(&self, msg: &str);
    /// Dim `+ cmd` echo. Printed before the command runs.
    fn plus(&self, cmd: &str);
    fn detail(&self, msg: &str);
    fn line(&self, msg: &str);
    fn blank(&self);
}
