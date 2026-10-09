//! Shared loss-recovery policy for the native NVENC, AMF, and QSV backends, read off the
//! session's [`knobs`](crate::knobs). Defaults stay with each backend (QSV LTR ~1/4 s, AMF
//! ~1/2 s — tuning, not drift). API clamps stay at the call site (QSV `mfxU16` 8..=240).
//! Sibling of `rfi.rs`, which owns the slot-recovery policy.

/// `PUNKTFUNK_INTRA_REFRESH=1` — opt into the periodic intra-refresh wave on
/// AMF/QSV: a moving intra band heals FEC-unrecoverable loss without a
/// 20-40× IDR spike, and selects IR over LTR there (the wave sweeps the
/// picture; LTR pins references). `0` also turns off the on-demand wave every
/// backend runs where an RFI declines (`rfi::wave_enabled`).
pub fn intra_refresh_requested() -> bool {
    crate::knobs::get().intra_refresh == 1
}

/// `PUNKTFUNK_IR_PERIOD_FRAMES` — wave length in frames (`>= 2` or it is not
/// a wave). Default is half a second of frames (~2-3 % intra cost per frame).
/// Backends clamp to their API field at the call site.
pub fn intra_refresh_period(fps: u32) -> u32 {
    match crate::knobs::get().ir_period_frames {
        n if n >= 2 => u32::from(n),
        _ => fps.max(16) / 2,
    }
}

/// `PUNKTFUNK_LTR_INTERVAL_FRAMES` — LTR mark cadence (`>= 1`). `None` leaves
/// the backend's tuned default; it does not disable LTR.
#[cfg(target_os = "windows")]
pub fn ltr_interval() -> Option<i64> {
    match crate::knobs::get().ltr_interval_frames {
        0 => None,
        n => Some(i64::from(n)),
    }
}

/// `PUNKTFUNK_LTR_FORCE_AT=N` — spike-only: at `frame_idx == N` the encoder
/// self-triggers `invalidate_ref_frames` so a headless run exercises LTR
/// recovery. `None` normally. N must be `> 0`; frame 0 is the opening IDR.
#[cfg(target_os = "windows")]
pub fn ltr_test_force_at() -> Option<i64> {
    match crate::knobs::get().ltr_force_at {
        0 => None,
        n => Some(i64::from(n)),
    }
}
