//! The guided punktfunk installer, as a library so the tests can drive every stage.
//!
//! `Facts → Choices → Plan → Execute`, strictly ordered: probe the box, derive the
//! editable option set, turn both into a pure `Vec<Step>`, then run it. `--dry-run`
//! is "render the Plan" by construction rather than a flag threaded through the call
//! sites, and `--demo` swaps the seams for fakes so nothing can touch the machine.
//!
//! `scripts/install.sh` is the behavioural spec. Traps that must port verbatim each
//! have a named test here. See `design/installer-v2.md`.

pub mod choices;
pub mod demo;
pub mod exec;
pub mod facts;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod plan;
pub mod platform;
pub mod report;
pub mod seam;
pub mod ui;
