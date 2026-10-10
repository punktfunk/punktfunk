//! Core error type and the C ABI status codes it maps to.
//!
//! `PunktfunkStatus` is stable: `Ok` is 0, errors are negative, existing
//! variants must not be renumbered. Rejection codes live in the -20 block.

use thiserror::Error;

/// Internal error. Crosses the C ABI as a [`PunktfunkStatus`] code.
#[derive(Debug, Error)]
pub enum PunktfunkError {
    #[error("invalid argument: {0}")]
    InvalidArg(&'static str),
    #[error("fec error: {0}")]
    Fec(#[from] crate::fec::FecError),
    #[error("crypto seal/open failed")]
    Crypto,
    #[error("malformed packet")]
    BadPacket,
    #[error("no complete frame available yet")]
    NoFrame,
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out")]
    Timeout,
    #[error("session closed")]
    Closed,
    /// Host turned this connection away with a typed
    /// [`crate::reject::RejectReason`]. Distinct from transport
    /// ([`Self::Io`] / [`Self::Timeout`]) and a failed PIN ([`Self::Crypto`]).
    #[error("rejected by host: {0}")]
    Rejected(crate::reject::RejectReason),
}

pub type Result<T> = core::result::Result<T, PunktfunkError>;

/// Stable C ABI status codes. `Ok` is 0; errors are negative so callers can
/// test `rc < 0`. Existing variants must not be renumbered — only append.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunktfunkStatus {
    Ok = 0,
    InvalidArg = -1,
    Fec = -2,
    Crypto = -3,
    BadPacket = -4,
    NoFrame = -5,
    Unsupported = -6,
    Io = -7,
    NullPointer = -8,
    Timeout = -9,
    Closed = -10,
    // -11..-19 reserved. The -20 block mirrors `RejectReason` one-to-one
    // so FFI callers can switch on the host's reason.
    RejectedNotArmed = -20,
    RejectedBoundOther = -21,
    RejectedRateLimited = -22,
    RejectedIdentityRequired = -23,
    RejectedDenied = -24,
    RejectedApprovalTimeout = -25,
    RejectedSuperseded = -26,
    RejectedWireVersion = -27,
    RejectedBusy = -28,
    RejectedSetupFailed = -29,
    RejectedAccessExpired = -30,
    RejectedLaunchNotPermitted = -31,
    RejectedHostPower = -32,
    RejectedProfileUnknown = -33,
    RejectedNoSeat = -34,
    RejectedSeatOccupied = -35,
    RejectedSeatUnavailable = -36,
    Panic = -99,
}

impl PunktfunkError {
    pub fn status(&self) -> PunktfunkStatus {
        match self {
            PunktfunkError::InvalidArg(_) => PunktfunkStatus::InvalidArg,
            PunktfunkError::Fec(_) => PunktfunkStatus::Fec,
            PunktfunkError::Crypto => PunktfunkStatus::Crypto,
            PunktfunkError::BadPacket => PunktfunkStatus::BadPacket,
            PunktfunkError::NoFrame => PunktfunkStatus::NoFrame,
            PunktfunkError::Unsupported(_) => PunktfunkStatus::Unsupported,
            PunktfunkError::Io(_) => PunktfunkStatus::Io,
            PunktfunkError::Timeout => PunktfunkStatus::Timeout,
            PunktfunkError::Closed => PunktfunkStatus::Closed,
            PunktfunkError::Rejected(r) => {
                use crate::reject::RejectReason as R;
                match r {
                    R::PairingNotArmed => PunktfunkStatus::RejectedNotArmed,
                    R::PairingBoundToOtherDevice => PunktfunkStatus::RejectedBoundOther,
                    R::PairingRateLimited => PunktfunkStatus::RejectedRateLimited,
                    R::IdentityRequired => PunktfunkStatus::RejectedIdentityRequired,
                    R::Denied => PunktfunkStatus::RejectedDenied,
                    R::ApprovalTimeout => PunktfunkStatus::RejectedApprovalTimeout,
                    R::Superseded => PunktfunkStatus::RejectedSuperseded,
                    R::WireVersionMismatch => PunktfunkStatus::RejectedWireVersion,
                    R::Busy => PunktfunkStatus::RejectedBusy,
                    R::SetupFailed => PunktfunkStatus::RejectedSetupFailed,
                    R::AccessExpired => PunktfunkStatus::RejectedAccessExpired,
                    R::LaunchNotPermitted => PunktfunkStatus::RejectedLaunchNotPermitted,
                    R::HostPower => PunktfunkStatus::RejectedHostPower,
                    R::ProfileUnknown => PunktfunkStatus::RejectedProfileUnknown,
                    R::NoSeat => PunktfunkStatus::RejectedNoSeat,
                    R::SeatOccupied => PunktfunkStatus::RejectedSeatOccupied,
                    R::SeatUnavailable => PunktfunkStatus::RejectedSeatUnavailable,
                }
            }
        }
    }
}
