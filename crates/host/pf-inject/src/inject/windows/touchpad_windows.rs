//! Virtual touchpad: a per-session Precision Touchpad through the `pf-touchpad` UMDF2 HID
//! minidriver, beside `pf-mouse` on the same sealed channel and session fence. Windows takes
//! touchpad input only from a HID device; synthetic pointers cover pen and touch alone.
//!
//! The host's contract, in the order it calls:
//! - [`supported`] and [`taps`] once per session at the handshake: `FEATURE_TOUCHPAD` when the
//!   driver is installed, `FEATURE_TOUCHPAD_TAPS` when "tap to click" is on for the user.
//! - [`VirtualTouchpad::create`] on the session's first frame: the devnode appears then. An
//!   error latches for the session.
//! - [`VirtualTouchpad::apply`] with the whole pad after every applied frame, in pad units:
//!   X `0..=TOUCHPAD_PAD_MAX_X`, Y `0..=TOUCHPAD_PAD_MAX_Y`, `TOUCHPAD_UNITS_PER_MM` per mm. A
//!   contact absent from the state has lifted; ids are unique and at most 31.
//! - Dropping the value removes the devnode, at session end and after a final all-lifted apply.
//!
//! No backend yet: no Windows host offers the feature. Design: `design/touchpad-passthrough.md`.

use anyhow::{bail, Result};
use punktfunk_core::quic::PadState;

pub fn supported() -> bool {
    false
}

pub fn taps() -> bool {
    false
}

pub struct VirtualTouchpad;

impl VirtualTouchpad {
    pub fn create() -> Result<VirtualTouchpad> {
        bail!("no precision touchpad backend in this build")
    }

    pub fn apply(&mut self, _pad: &PadState) {}
}
