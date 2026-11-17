//! The guided punktfunk installer, as a library so the tests can drive every stage.
//!
//! `Facts → Choices → Plan → Execute`, strictly ordered: probe the box, derive the
//! editable option set, turn both into a pure `Vec<Step>`, then run it. `--dry-run`
//! is "render the Plan" by construction rather than a flag threaded through the call
//! sites, and `--demo` swaps the seams for fakes so nothing can touch the machine.
//!
//! `scripts/install.sh` only downloads this binary and execs it; `design/installer-v2.md` is
//! the contract. Traps that must port verbatim each have a named test here.

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
