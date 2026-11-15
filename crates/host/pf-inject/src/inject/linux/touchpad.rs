//! Virtual touchpad: a per-session uinput multitouch touchpad (`INPUT_PROP_POINTER`,
//! `INPUT_PROP_BUTTONPAD`, MT slots, resolution set) that libinput turns into pointer motion,
//! scrolling and gestures for the compositor. Gated like the pen: `/dev/uinput` opens, the
//! node is not a seat proxy, the session is not gamescope (the handshake checks that last).
//!
//! The host's contract, in the order it calls:
//! - [`supported`] and [`taps`] once per session at the handshake: `FEATURE_TOUCHPAD` and
//!   `FEATURE_TOUCHPAD_TAPS`. Taps stay off while the compositor owns tap-to-click.
//! - [`VirtualTouchpad::create`] on the session's first frame. An error latches for the session.
//! - [`VirtualTouchpad::apply`] with the whole pad after every applied frame, in pad units:
//!   X `0..=TOUCHPAD_PAD_MAX_X`, Y `0..=TOUCHPAD_PAD_MAX_Y`, `TOUCHPAD_UNITS_PER_MM` per mm. A
//!   contact absent from the state has lifted; ids are unique and at most 31.
//! - Dropping the value removes the device, at session end and after a final all-lifted apply.
//!
//! No backend yet: no Linux host offers the feature. Design: `design/touchpad-passthrough.md`.

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
        bail!("no uinput touchpad backend in this build")
    }

    pub fn apply(&mut self, _pad: &PadState) {}
}
