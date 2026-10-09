//! Host wall clock in unix time. Stored access deadlines and every check against them, grant
//! stamps and event timestamps read this one clock, so an NTP step moves a deadline with it.
//! `sleep_inhibit` keeps its own monotonic clock on purpose.

use punktfunk_core::quic::wall_clock_ns;

/// Unix seconds; 0 before the epoch.
pub(crate) fn unix_secs() -> i64 {
    (wall_clock_ns() / 1_000_000_000) as i64
}

/// [`unix_secs`] for the fields stored unsigned.
pub(crate) fn unix_secs_u64() -> u64 {
    wall_clock_ns() / 1_000_000_000
}

/// Unix milliseconds; 0 before the epoch. The event bus, status API and logs stamp in this.
pub(crate) fn unix_ms() -> u64 {
    wall_clock_ns() / 1_000_000
}
