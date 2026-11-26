//! The Windows platform: one local account and one loopback Remote Desktop session per seat.
//!
//! The supervisor runs inside the punktfunk service, as LocalSystem. To start a seat it signs the
//! seat's account in over Remote Desktop to this same machine, through a small RDP client called
//! the keeper. It then checks that the new session can stream and starts the seat's host there.
//!
//! Where to start reading:
//!
//! - [`step`]: a seat's whole lifecycle as a pure function. State and one observation go in,
//!   actions come out. `supervisor` performs the actions.
//! - [`switch`]: turning seats on and off for the machine, as an ordered plan. `enable` performs
//!   it.
//! - [`lent`]: which session each lent USB controller belongs to.
//! - [`doctor`]: the rows of the seat check, decided from facts that were already read.
//! - `backend`: `WindowsBackend`, which carries out the portable commands with all of these.
//!
//! The first four call no Windows API, so they and their tests build on every target. The rest
//! builds on Windows only.

#[cfg(windows)]
mod accounts;
#[cfg(windows)]
mod backend;
#[cfg(windows)]
mod credentials;
pub(crate) mod doctor;
#[cfg(windows)]
mod enable;
#[cfg(windows)]
pub mod keeper;
pub mod lent;
#[cfg(windows)]
pub mod pipe;
#[cfg(windows)]
mod process;
#[cfg(windows)]
mod rdp;
#[cfg(windows)]
mod seat_listener;
#[cfg(windows)]
mod steam;
pub(crate) mod step;
#[cfg(windows)]
mod supervisor;
pub(crate) mod switch;
#[cfg(windows)]
mod util;
#[cfg(windows)]
mod wts;

#[cfg(windows)]
pub use accounts::{seats_group_sid, SEATS_GROUP};
#[cfg(windows)]
pub use backend::{
    account_profile, seats_enabled, server_edition, session_wrapper, turn_off, WindowsBackend,
};
