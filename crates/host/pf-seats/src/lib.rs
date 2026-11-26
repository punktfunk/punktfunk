//! The seat supervisor: it creates, starts and stops the seats of one machine, up to four.
//!
//! A seat is one player's own desktop: an OS account, a session and a `punktfunk-host serve` of
//! its own. `README.md` beside this crate's `Cargo.toml` maps who runs as whom. `CONTEXT.md` at
//! the repo root defines the words.
//!
//! The portable half is the saved list of seats ([`model`]), its crash-safe file
//! ([`persistence`]), the commands ([`ipc`]) and their one-at-a-time dispatch ([`service`]).
//! Everything that touches the machine goes through [`PlatformBackend`], so the portable tests
//! run on a fake.
//!
//! - Linux (`linux`): one user, one logind session and one systemd unit per seat, run by the
//!   root daemon `punktfunk-seats` behind a Unix socket.
//! - Windows (`windows`): one local account and one loopback Remote Desktop session per seat,
//!   run inside the punktfunk service behind a named pipe.

// Off Windows, `unsafe` is allowed in four places only: `linux::pam` (libpam), `linux::login`
// (signals and the privilege drop), the `SO_PEERPIDFD` read in `linux::handover` and the
// `bpf(2)` call in `linux::loopback`.
#![cfg_attr(not(windows), deny(unsafe_code))]

pub mod attempts;
pub mod backend;
pub mod backoff;
#[cfg(any(windows, test))]
pub mod bootstrap;
/// The Windows doctor's row decisions; the tables run on every target.
#[cfg(any(windows, test))]
mod doctor;
pub mod ipc;
/// The Linux half. Its tests run wherever Unix does; only the daemon needs Linux.
#[cfg(any(target_os = "linux", all(test, unix)))]
pub mod linux;
pub mod locks;
pub mod logging;
pub mod model;
pub mod persistence;
pub mod pin;
/// Desk pads lent to a seat on Windows, declared here so its tests run on every target.
#[path = "windows/lent.rs"]
pub mod seat_pads;
/// The Windows supervisor's pure half, declared here so its tests run on every target.
#[path = "windows/step.rs"]
mod seat_step;
/// Seats on and off on Windows as a pure plan, declared here so its tests run on every target.
#[path = "windows/switch.rs"]
mod seat_switch;
mod secret;
pub mod service;
pub mod steam;
pub mod unlock;
#[cfg(windows)]
pub mod windows;

pub use backend::{BackendError, PlatformBackend, PresentPad, UnsupportedBackend};
pub use ipc::{Command, CommandResult, Request, Response};
#[cfg(target_os = "linux")]
pub use linux::LinuxBackend;
pub use model::{CreateSeat, Ledger, LentPad, RuntimeState, RuntimeStatus, Seat, SeatId};
pub use persistence::{LedgerStore, SecretRoot, StoreError};
pub use secret::Secret;
pub use service::SeatService;
#[cfg(windows)]
pub use windows::WindowsBackend;
