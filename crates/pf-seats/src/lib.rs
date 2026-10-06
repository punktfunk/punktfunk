//! The seat supervisor: a ledger of up to four seats and the platform work behind it.
//!
//! The portable core owns the validated ledger, the crash-recoverable secret root, the control
//! protocol and serialized command dispatch. Platform effects cross `PlatformBackend`, so
//! portable tests use a fake. Windows adds account ownership, DPAPI credentials, NLA session
//! keepers, WTS selection, job-contained supervision and the named pipe the punktfunk service
//! serves. Linux adds users with their own logind session, one systemd unit per seat and the
//! Unix socket the `punktfunk-seats` daemon serves.

#![cfg_attr(not(windows), forbid(unsafe_code))]

pub mod backend;
#[cfg(any(windows, test))]
pub mod bootstrap;
pub mod ipc;
/// The Linux half. Its tests run wherever Unix does; only the daemon needs Linux.
#[cfg(any(target_os = "linux", all(test, unix)))]
pub mod linux;
pub mod logging;
pub mod model;
pub mod persistence;
pub mod pin;
pub mod service;
#[cfg(windows)]
pub mod windows;

pub use backend::{BackendError, PlatformBackend, UnsupportedBackend};
pub use ipc::{Command, CommandResult, Request, Response};
#[cfg(target_os = "linux")]
pub use linux::LinuxBackend;
pub use model::{CreateSeat, Ledger, RuntimeState, RuntimeStatus, Seat, SeatId};
pub use persistence::{LedgerStore, SecretRoot, StoreError};
pub use service::SeatService;
#[cfg(windows)]
pub use windows::WindowsBackend;
