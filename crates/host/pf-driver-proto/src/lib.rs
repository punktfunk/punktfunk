//! Shared binary contract between the punktfunk host and the `pf-vdisplay` IddCx driver.
//!
//! Two planes:
//! * [`control`] — `DeviceIoControl` (add/remove, adapter pin, keepalive, info, clear-all,
//!   cursor delivery). Owned and versioned — not the SudoVDA ABI.
//! * [`encode`] — the video transport. The host creates an unnamed section plus a ready event,
//!   duplicates the handles into WUDFHost over [`encode::IOCTL_SET_ENCODE`], and the driver's
//!   encoder publishes access units into it. No object-name scheme: unnamed objects cannot be
//!   enumerated, opened by name, or squatted. This crate owns [`encode::au::AuHeader`], the
//!   [`encode::FrameToken`] publish cell and the status codes.
//!   Evidence: `design/idd-push-security.md`.
//!
//! GUID and LUID travel as integers; each side converts to its own `windows` / bindgen types.
//! `Pod` + `offset_of!` asserts make a one-sided layout edit a compile error.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

/// Device-interface GUID `{70667664-7044-5350-a1b2-c3d4e5f60001}`.
/// Not SudoVDA's `{e5bcc234-…}`: a private GUID so a real SudoVDA install cannot bind.
/// Construct via `GUID::from_u128(PF_VDISPLAY_INTERFACE_GUID_U128)`.
pub const PF_VDISPLAY_INTERFACE_GUID_U128: u128 = 0x7066_7664_7044_5350_a1b2_c3d4_e5f6_0001;

/// `(Data1, Data2, Data3, Data4)` of [`PF_VDISPLAY_INTERFACE_GUID_U128`].
/// This crate is `no_std` and has no `GUID` type; callers rebuild theirs from these fields.
#[must_use]
pub const fn interface_guid_fields() -> (u32, u16, u16, [u8; 8]) {
    let g = PF_VDISPLAY_INTERFACE_GUID_U128;
    (
        (g >> 96) as u32,
        (g >> 80) as u16,
        (g >> 64) as u16,
        (g as u64).to_be_bytes(),
    )
}

/// Bumped on any incompatible change to either plane. Exchanged via [`control::IOCTL_GET_INFO`];
/// host and driver assert a match at startup.
///
/// v9 widens [`encode::au::AuSlot`] from 32 to 48 bytes with the driver's encode-submit and
/// publish QPC, so the host can split present → arrival into pool, encode and hand-off. A layout
/// change, so not additive; host and driver ship in one installer.
///
/// v8 scopes the driver to the process that asks: a monitor, its encoder and its cursor channel
/// answer only to the owner whose `IOCTL_ADD` created them, `IOCTL_CLEAR_ALL` departs the
/// caller's own, and an owner's monitors depart when its last control handle closes or it goes
/// silent for the watchdog window. `IOCTL_SET_RENDER_ADAPTER` stays adapter-wide by IddCx
/// design. v7 replaced the pixel ring with driver-side encode into the host's section
/// ([`encode::IOCTL_SET_ENCODE`]); `IOCTL_SET_FRAME_CHANNEL` no longer exists.
///
/// [`control::AddRequest`] luminance tail and [`control::AddReply::cursor_excluded`] are
/// prefix-compatible (no bump): a short read/write sees zeros = unknown. A hardware-cursor
/// declare is irrevocable on the adapter — DWM excludes the pointer from every later monitor
/// until the adapter resets — so the driver blends the pointer when the client draws none.
pub const PROTOCOL_VERSION: u32 = 9;

/// Oldest driver this host still drives. Equal to [`PROTOCOL_VERSION`]: a v8 driver writes
/// 32-byte slots into a table this host lays out at 48, v7 lets a second host reach this
/// host's monitors, and v6 speaks a video transport that no longer exists.
pub const MIN_DRIVER_PROTOCOL_VERSION: u32 = 9;

/// `CTL_CODE(FILE_DEVICE_UNKNOWN = 0x22, func, METHOD_BUFFERED = 0, FILE_ANY_ACCESS = 0)`.
pub const fn ctl_code(func: u32) -> u32 {
    (0x22u32 << 16) | (func << 2)
}

// Virtual display: control IOCTLs, EDID, mode lists, encode transport, cursor channel.
pub mod control;
pub mod cursor;
pub mod edid;
pub mod encode;
pub mod vdisplay;

// The capture worker's control channel: the encode messages above, over a pipe.
pub mod worker;

// Input devices: the pad channel, per-pad HID tables and the virtual mouse.
pub mod deck;
pub mod dualsense;
pub mod dualshock4;
pub mod eightbitdo;
pub mod gamepad;
pub mod hori;
pub mod mouse;
pub mod rdesc;
pub mod switch;
pub mod triton;
pub mod xbox;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctl_codes_are_contiguous_and_distinct() {
        assert_eq!(control::IOCTL_ADD, ctl_code(0x900));
        let all = [
            control::IOCTL_ADD,
            control::IOCTL_REMOVE,
            control::IOCTL_SET_RENDER_ADAPTER,
            control::IOCTL_PING,
            control::IOCTL_GET_INFO,
            control::IOCTL_CLEAR_ALL,
            control::IOCTL_UPDATE_MODES,
            control::IOCTL_SET_CURSOR_CHANNEL,
            control::IOCTL_SET_CURSOR_FORWARD,
            control::IOCTL_ENCODE_PROBE_ARM,
            control::IOCTL_ENCODE_PROBE_STATUS,
            encode::IOCTL_SET_ENCODE,
            encode::IOCTL_ENCODE_CTL,
            control::IOCTL_DRAIN_LOG,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(encode::IOCTL_ENCODE_CTL, ctl_code(0x90D));
        assert_eq!(control::IOCTL_DRAIN_LOG, ctl_code(0x90E));
    }

    #[test]
    fn guid_is_not_sudovda() {
        const SUDOVDA: u128 = 0xE5BC_C234_1E0C_418A_A0D4_EF8B_7501_414D;
        assert_ne!(PF_VDISPLAY_INTERFACE_GUID_U128, SUDOVDA);
    }
}
