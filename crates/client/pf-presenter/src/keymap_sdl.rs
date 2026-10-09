//! SDL scancodes / mouse buttons → the punktfunk input wire contract.
//!
//! Same Windows VK codes as `pf_client_core::keymap` (GameStream wire). Keyed on
//! SDL scancodes, not evdev: SDL maps the platform keycode onto its USB-HID
//! scancode table, so this stays layout-independent like the evdev table.
//! Coverage mirrors `keymap::evdev_to_vk` one-to-one. A key the wire does not
//! cover (media keys, etc.) returns `None` and is dropped; never guessed.

use sdl3::keyboard::Scancode;

pub fn scancode_to_vk(sc: Scancode) -> Option<u8> {
    use Scancode as S;
    Some(match sc {
        S::Backspace => 0x08,
        S::Tab => 0x09,
        S::Return => 0x0D,
        S::Pause => 0x13,
        S::CapsLock => 0x14,
        S::Escape => 0x1B,
        S::Space => 0x20,
        S::PageUp => 0x21,
        S::PageDown => 0x22,
        S::End => 0x23,
        S::Home => 0x24,
        S::Left => 0x25,
        S::Up => 0x26,
        S::Right => 0x27,
        S::Down => 0x28,
        S::PrintScreen => 0x2C,
        S::Insert => 0x2D,
        S::Delete => 0x2E,

        S::_0 => 0x30,
        S::_1 => 0x31,
        S::_2 => 0x32,
        S::_3 => 0x33,
        S::_4 => 0x34,
        S::_5 => 0x35,
        S::_6 => 0x36,
        S::_7 => 0x37,
        S::_8 => 0x38,
        S::_9 => 0x39,

        S::A => 0x41,
        S::B => 0x42,
        S::C => 0x43,
        S::D => 0x44,
        S::E => 0x45,
        S::F => 0x46,
        S::G => 0x47,
        S::H => 0x48,
        S::I => 0x49,
        S::J => 0x4A,
        S::K => 0x4B,
        S::L => 0x4C,
        S::M => 0x4D,
        S::N => 0x4E,
        S::O => 0x4F,
        S::P => 0x50,
        S::Q => 0x51,
        S::R => 0x52,
        S::S => 0x53,
        S::T => 0x54,
        S::U => 0x55,
        S::V => 0x56,
        S::W => 0x57,
        S::X => 0x58,
        S::Y => 0x59,
        S::Z => 0x5A,

        S::LGui => 0x5B,
        S::RGui => 0x5C,
        S::Application => 0x5D,

        S::Kp0 => 0x60,
        S::Kp1 => 0x61,
        S::Kp2 => 0x62,
        S::Kp3 => 0x63,
        S::Kp4 => 0x64,
        S::Kp5 => 0x65,
        S::Kp6 => 0x66,
        S::Kp7 => 0x67,
        S::Kp8 => 0x68,
        S::Kp9 => 0x69,
        S::KpMultiply => 0x6A,
        S::KpPlus => 0x6B,
        // KP-Enter → 0x6C (VK_SEPARATOR), matching KEY_KPENTER — not VK_RETURN.
        S::KpEnter => 0x6C,
        S::KpMinus => 0x6D,
        S::KpPeriod => 0x6E,
        S::KpDivide => 0x6F,

        S::F1 => 0x70,
        S::F2 => 0x71,
        S::F3 => 0x72,
        S::F4 => 0x73,
        S::F5 => 0x74,
        S::F6 => 0x75,
        S::F7 => 0x76,
        S::F8 => 0x77,
        S::F9 => 0x78,
        S::F10 => 0x79,
        S::F11 => 0x7A,
        S::F12 => 0x7B,

        S::NumLockClear => 0x90,
        S::ScrollLock => 0x91,

        S::LShift => 0xA0,
        S::RShift => 0xA1,
        S::LCtrl => 0xA2,
        S::RCtrl => 0xA3,
        S::LAlt => 0xA4,
        S::RAlt => 0xA5,

        // OEM VKs are US-layout positions, not glyphs.
        S::Semicolon => 0xBA,
        S::Equals => 0xBB,
        S::Comma => 0xBC,
        S::Minus => 0xBD,
        S::Period => 0xBE,
        S::Slash => 0xBF,
        S::Grave => 0xC0,
        S::LeftBracket => 0xDB,
        S::Backslash => 0xDC,
        S::RightBracket => 0xDD,
        S::Apostrophe => 0xDE,
        S::NonUsBackslash => 0xE2,

        // IME keys, as `keymap::evdev_to_vk`. JIS 半角/全角 sits on Grave and stays VK_OEM_3.
        S::Lang1 => 0x15,
        S::Lang2 => 0x19,
        S::International4 => 0x1C,
        S::International5 => 0x1D,
        S::International2 => 0xF2,
        S::International1 => 0xC1, // JIS ろ, ABNT2 /?
        S::KpComma => 0xC2,        // ABNT2 keypad .
        S::International3 => 0xE1, // JIS ¥

        _ => return None,
    })
}

/// SDL twin of `keymap::gdk_button_to_gs` — same GameStream button ids.
pub fn mouse_button_to_gs(b: sdl3::mouse::MouseButton) -> Option<u32> {
    use sdl3::mouse::MouseButton as B;
    Some(match b {
        B::Left => 1,
        B::Middle => 2,
        B::Right => 3,
        B::X1 => 4,
        B::X2 => 5,
        _ => return None,
    })
}

// The evdev table only exists on Linux; the SDL table under test is cross-platform.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use pf_client_core::keymap::evdev_to_vk;

    /// Same physical key → same VK as `evdev_to_vk`.
    #[test]
    fn agrees_with_the_evdev_table() {
        use Scancode as S;
        let same_key: &[(Scancode, u16)] = &[
            (S::Backspace, 14),
            (S::Tab, 15),
            (S::Return, 28),
            (S::Escape, 1),
            (S::Space, 57),
            (S::PageUp, 104),
            (S::Left, 105),
            (S::_0, 11),
            (S::_1, 2),
            (S::_9, 10),
            (S::A, 30),
            (S::Q, 16),
            (S::Z, 44),
            (S::LGui, 125),
            (S::Kp0, 82),
            (S::Kp9, 73),
            (S::KpEnter, 96),
            (S::F1, 59),
            (S::F11, 87),
            (S::F12, 88),
            (S::NumLockClear, 69),
            (S::LShift, 42),
            (S::RAlt, 100),
            (S::Semicolon, 39),
            (S::Grave, 41),
            (S::NonUsBackslash, 86),
            (S::Lang1, 122),
            (S::Lang2, 123),
            (S::International4, 92),
            (S::International5, 94),
            (S::International2, 93),
            (S::International1, 89),
            (S::KpComma, 121),
            (S::International3, 124),
        ];
        for &(sc, ev) in same_key {
            assert_eq!(scancode_to_vk(sc), evdev_to_vk(ev), "scancode {sc:?}");
        }
        assert_eq!(scancode_to_vk(Scancode::Mute), None); // not in the wire contract
    }
}
