//! The audited unsafe-primitive layer under the punktfunk UMDF drivers.
//!
//! A UMDF driver cannot be literally free of `unsafe` — WDF dispatch, Win32 section mapping and
//! cross-process shared memory are FFI by nature. What Rust buys is confining every raw operation
//! to one small, reviewed layer with explicit contracts, so the drivers' business logic (the
//! sealed-channel state machine, report plumbing, IOCTL policy) is **100 % safe code** and a
//! memory-safety bug can only live here.
//!
//! * [`section`] — [`section::MappedView`]: bounds- and alignment-checked access to a mapped
//!   section (atomics for the sync fields), plus the leaked-view [`section::ViewCell`].
//! * [`channel`] — [`channel::ChannelClient`]: the sealed pad channel's driver side
//!   (`design/gamepad-channel-sealing.md`), a **`#[forbid(unsafe_code)]` module**.
//! * [`wdf`] — [`wdf::Request`] + queue/device-property helpers: each callback converts its raw
//!   `WDFREQUEST` into a token exactly once; everything after that is safe.
//! * [`hid`] — the HID IOCTL table, descriptor and string helpers (no `unsafe`), and the notes on
//!   how a driver tells the host which process serves its devnode over the device stack rather
//!   than the LocalService-writable bootstrap mailbox.
//! * [`log`] — [`log::FileLog`] plus [`file_log!`] / [`dbglog!`]: the opt-in bring-up file log.
//! * [`skeleton`] — `DriverEntry`, both queues and the periodic timer the HID drivers share.
//!
//! Lint gates (workspace-wide, enforced by the drivers CI clippy step): `unsafe_op_in_unsafe_fn`
//! + `clippy::undocumented_unsafe_blocks`.

pub mod channel;
pub mod hid;
pub mod log;
pub mod section;
pub mod skeleton;
pub mod wdf;

/// `NT_SUCCESS` — an NTSTATUS is an error iff negative.
#[inline]
#[must_use]
pub const fn nt_success(status: wdk_sys::NTSTATUS) -> bool {
    status >= 0
}
