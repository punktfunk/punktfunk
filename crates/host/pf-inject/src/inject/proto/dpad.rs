//! Wire D-pad bits → the 8-way hat every HID pad reports. Each codec maps the centred
//! `None` to its own null value.

use punktfunk_core::input::gamepad as gs;

/// Octant 0..7 clockwise from North, `None` when centred. Opposing presses cancel first, so
/// up+down+left is West: a physical hat cannot report both.
pub(crate) fn dpad_octant(buttons: u32) -> Option<u8> {
    let on = |bit: u32| (buttons & bit != 0) as i8;
    let x = on(gs::BTN_DPAD_RIGHT) - on(gs::BTN_DPAD_LEFT);
    let y = on(gs::BTN_DPAD_DOWN) - on(gs::BTN_DPAD_UP);
    match (x, y) {
        (0, -1) => Some(0),
        (1, -1) => Some(1),
        (1, 0) => Some(2),
        (1, 1) => Some(3),
        (0, 1) => Some(4),
        (-1, 1) => Some(5),
        (-1, 0) => Some(6),
        (-1, -1) => Some(7),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dualsense_proto::DsState;
    use crate::eightbitdo_proto::{EightBitDoState, Model};
    use crate::hori_proto::HoriState;
    use crate::xbox_proto::{serialize_xbox_state, XboxState};
    use punktfunk_core::input::GamepadFrame;

    const U: u32 = gs::BTN_DPAD_UP;
    const D: u32 = gs::BTN_DPAD_DOWN;
    const L: u32 = gs::BTN_DPAD_LEFT;
    const R: u32 = gs::BTN_DPAD_RIGHT;

    /// Each codec's hat for `buttons`, in its own encoding: 8BitDo, HORI, Xbox, DualSense.
    fn hats(buttons: u32) -> [u8; 4] {
        let f = GamepadFrame {
            buttons,
            ..Default::default()
        };
        let xbox = serialize_xbox_state(&XboxState::from_gamepad(buttons, 0, 0, 0, 0, 0, 0));
        [
            EightBitDoState::merge_frame(Model::Ultimate2, &EightBitDoState::neutral(), &f).hat,
            HoriState::merge_frame(&HoriState::neutral(), &f).hat,
            xbox[13] & 0x0F,
            DsState::from_gamepad(buttons, 0, 0, 0, 0, 0, 0).dpad,
        ]
    }

    #[test]
    fn the_octant_walks_clockwise_from_north() {
        let walk = [U, U | R, R, R | D, D, D | L, L, U | L];
        for (i, b) in walk.into_iter().enumerate() {
            assert_eq!(dpad_octant(b), Some(i as u8), "buttons {b:#x}");
        }
        assert_eq!(dpad_octant(0), None);
    }

    /// 8BitDo and HORI centre on `0x0F`, Xbox on `0` (then 1..8), the DualSense family on `8`.
    #[test]
    fn opposing_dpad_presses_cancel() {
        for b in [U | D, L | R, U | D | L | R] {
            assert_eq!(hats(b), [0x0F, 0x0F, 0, 8], "buttons {b:#x}");
        }
        assert_eq!(hats(U | D | L), [6, 6, 7, 6], "up+down+left keeps West");
        assert_eq!(hats(L | R | D), [4, 4, 5, 4], "left+right+down keeps South");
    }
}
