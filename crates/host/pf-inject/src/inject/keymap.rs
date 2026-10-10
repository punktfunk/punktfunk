//! Windows Virtual-Key → Linux evdev, and GameStream mouse button → `BTN_*`.
//!
//! The Windows SendInput positional table must stay bit-for-bit with [`vk_to_evdev`].
//! [`KEY_FLAG_SEMANTIC_VK`] is in-process only. No state, no OS handles.

/// High bit of `flags`: `code` is a layout-semantic VK (GameStream), not US-positional.
///
/// Windows maps semantic VKs through the foreground layout and positional VKs through a
/// fixed table; mixing them swaps y/z on German layouts. Set by `gamestream::input::decode`.
/// The punktfunk/1 ingest strips this bit from wire events.
pub const KEY_FLAG_SEMANTIC_VK: u32 = 0x8000_0000;

/// Win32 `VK_PAUSE`. Shares set-1 scan 0x45 with NumLock; SendInput uses `wVk` (E1, not E0).
pub(crate) const VK_PAUSE: u16 = 0x13;

/// VKs whose set-1 make is E0-prefixed when `MapVirtualKeyExW` leaves the high bits
/// clear. NumLock (0x90) is plain 0x45. Pause ([`VK_PAUSE`]) is E1 — not this list.
pub(crate) fn vk_forced_extended(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5C | 0x5D | 0x6C | 0xA3 | 0xA5
    )
}

pub fn vk_to_evdev(vk: u8) -> Option<u16> {
    match vk {
        0x08 => Some(14),  // VK_BACK     -> KEY_BACKSPACE
        0x09 => Some(15),  // VK_TAB      -> KEY_TAB
        0x0D => Some(28),  // VK_RETURN   -> KEY_ENTER
        0x13 => Some(119), // VK_PAUSE    -> KEY_PAUSE
        0x14 => Some(58),  // VK_CAPITAL  -> KEY_CAPSLOCK
        0x1B => Some(1),   // VK_ESCAPE   -> KEY_ESC
        0x20 => Some(57),  // VK_SPACE    -> KEY_SPACE
        0x21 => Some(104), // VK_PRIOR    -> KEY_PAGEUP
        0x22 => Some(109), // VK_NEXT     -> KEY_PAGEDOWN
        0x23 => Some(107), // VK_END      -> KEY_END
        0x24 => Some(102), // VK_HOME     -> KEY_HOME
        0x25 => Some(105), // VK_LEFT     -> KEY_LEFT
        0x26 => Some(103), // VK_UP       -> KEY_UP
        0x27 => Some(106), // VK_RIGHT    -> KEY_RIGHT
        0x28 => Some(108), // VK_DOWN     -> KEY_DOWN
        0x2C => Some(99),  // VK_SNAPSHOT -> KEY_SYSRQ
        0x2D => Some(110), // VK_INSERT   -> KEY_INSERT
        0x2E => Some(111), // VK_DELETE   -> KEY_DELETE

        0xB0 => Some(163), // VK_MEDIA_NEXT_TRACK -> KEY_NEXTSONG
        0xB1 => Some(165), // VK_MEDIA_PREV_TRACK -> KEY_PREVIOUSSONG
        0xB2 => Some(166), // VK_MEDIA_STOP       -> KEY_STOPCD
        0xB3 => Some(164), // VK_MEDIA_PLAY_PAUSE -> KEY_PLAYPAUSE

        0x10 => Some(42), // VK_SHIFT   -> KEY_LEFTSHIFT
        0x11 => Some(29), // VK_CONTROL -> KEY_LEFTCTRL
        0x12 => Some(56), // VK_MENU    -> KEY_LEFTALT

        // KEY_0 is 11; KEY_1..KEY_9 are 2..10.
        0x30 => Some(11), // VK_0
        0x31 => Some(2),  // VK_1
        0x32 => Some(3),  // VK_2
        0x33 => Some(4),  // VK_3
        0x34 => Some(5),  // VK_4
        0x35 => Some(6),  // VK_5
        0x36 => Some(7),  // VK_6
        0x37 => Some(8),  // VK_7
        0x38 => Some(9),  // VK_8
        0x39 => Some(10), // VK_9

        // A-Z evdev codes are not sequential.
        0x41 => Some(30), // A
        0x42 => Some(48), // B
        0x43 => Some(46), // C
        0x44 => Some(32), // D
        0x45 => Some(18), // E
        0x46 => Some(33), // F
        0x47 => Some(34), // G
        0x48 => Some(35), // H
        0x49 => Some(23), // I
        0x4A => Some(36), // J
        0x4B => Some(37), // K
        0x4C => Some(38), // L
        0x4D => Some(50), // M
        0x4E => Some(49), // N
        0x4F => Some(24), // O
        0x50 => Some(25), // P
        0x51 => Some(16), // Q
        0x52 => Some(19), // R
        0x53 => Some(31), // S
        0x54 => Some(20), // T
        0x55 => Some(22), // U
        0x56 => Some(47), // V
        0x57 => Some(17), // W
        0x58 => Some(45), // X
        0x59 => Some(21), // Y
        0x5A => Some(44), // Z

        0x5B => Some(125), // VK_LWIN -> KEY_LEFTMETA
        0x5C => Some(126), // VK_RWIN -> KEY_RIGHTMETA
        0x5D => Some(127), // VK_APPS -> KEY_COMPOSE

        0x60 => Some(82), // KP0
        0x61 => Some(79), // KP1
        0x62 => Some(80), // KP2
        0x63 => Some(81), // KP3
        0x64 => Some(75), // KP4
        0x65 => Some(76), // KP5
        0x66 => Some(77), // KP6
        0x67 => Some(71), // KP7
        0x68 => Some(72), // KP8
        0x69 => Some(73), // KP9
        0x6A => Some(55), // VK_MULTIPLY  -> KEY_KPASTERISK
        0x6B => Some(78), // VK_ADD       -> KEY_KPPLUS
        0x6C => Some(96), // VK_SEPARATOR -> KEY_KPENTER
        0x6D => Some(74), // VK_SUBTRACT  -> KEY_KPMINUS
        0x6E => Some(83), // VK_DECIMAL   -> KEY_KPDOT
        0x6F => Some(98), // VK_DIVIDE    -> KEY_KPSLASH

        // F1..F10 = 59..68; F11/F12 = 87/88.
        0x70 => Some(59),
        0x71 => Some(60),
        0x72 => Some(61),
        0x73 => Some(62),
        0x74 => Some(63),
        0x75 => Some(64),
        0x76 => Some(65),
        0x77 => Some(66),
        0x78 => Some(67),
        0x79 => Some(68),
        0x7A => Some(87),
        0x7B => Some(88),

        0x90 => Some(69), // VK_NUMLOCK -> KEY_NUMLOCK
        0x91 => Some(70), // VK_SCROLL  -> KEY_SCROLLLOCK

        0xA0 => Some(42),  // VK_LSHIFT   -> KEY_LEFTSHIFT
        0xA1 => Some(54),  // VK_RSHIFT   -> KEY_RIGHTSHIFT
        0xA2 => Some(29),  // VK_LCONTROL -> KEY_LEFTCTRL
        0xA3 => Some(97),  // VK_RCONTROL -> KEY_RIGHTCTRL
        0xA4 => Some(56),  // VK_LMENU    -> KEY_LEFTALT
        0xA5 => Some(100), // VK_RMENU    -> KEY_RIGHTALT

        // OEM VKs are US-layout positions.
        0xBA => Some(39), // VK_OEM_1      -> KEY_SEMICOLON
        0xBB => Some(13), // VK_OEM_PLUS   -> KEY_EQUAL
        0xBC => Some(51), // VK_OEM_COMMA  -> KEY_COMMA
        0xBD => Some(12), // VK_OEM_MINUS  -> KEY_MINUS
        0xBE => Some(52), // VK_OEM_PERIOD -> KEY_DOT
        0xBF => Some(53), // VK_OEM_2      -> KEY_SLASH
        0xC0 => Some(41), // VK_OEM_3      -> KEY_GRAVE
        0xDB => Some(26), // VK_OEM_4      -> KEY_LEFTBRACE
        0xDC => Some(43), // VK_OEM_5      -> KEY_BACKSLASH
        0xDD => Some(27), // VK_OEM_6      -> KEY_RIGHTBRACE
        0xDE => Some(40), // VK_OEM_7      -> KEY_APOSTROPHE
        0xE2 => Some(86), // VK_OEM_102    -> KEY_102ND

        // IME keys. Windows gives Korean and Japanese the same two VKs (HANGUL = KANA,
        // HANJA = KANJI); the clients send the JIS toggles as the DBE codes so the two
        // stay apart here. Positional on Windows too — `MapVirtualKeyExW` only knows them
        // under a Korean or Japanese layout.
        0x15 => Some(122), // VK_HANGUL       -> KEY_HANGEUL
        0x19 => Some(123), // VK_HANJA        -> KEY_HANJA
        0x1C => Some(92),  // VK_CONVERT      -> KEY_HENKAN
        0x1D => Some(94),  // VK_NONCONVERT   -> KEY_MUHENKAN
        0xF2 => Some(93),  // VK_DBE_HIRAGANA -> KEY_KATAKANAHIRAGANA
        0xF3 => Some(85),  // VK_DBE_SBCSCHAR -> KEY_ZENKAKUHANKAKU

        // The three keys a US board lacks. Windows reuses OEM_5/OEM_102 for them, so the
        // wire borrows the ABNT and AX labels instead; the scancode is what gets injected.
        0xC1 => Some(89),  // VK_ABNT_C1 -> KEY_RO       (JIS ろ, ABNT2 /?)
        0xC2 => Some(121), // VK_ABNT_C2 -> KEY_KPCOMMA  (ABNT2 keypad .)
        0xE1 => Some(124), // VK_OEM_AX  -> KEY_YEN      (JIS ¥)

        _ => None,
    }
}

/// GameStream button ids are not evdev order: 2 is middle, 3 is right.
#[cfg(target_os = "linux")]
pub(crate) fn gs_button_to_evdev(b: u32) -> Option<u32> {
    Some(match b {
        1 => 0x110, // BTN_LEFT
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        4 => 0x113, // BTN_SIDE  (X1)
        5 => 0x114, // BTN_EXTRA (X2)
        _ => return None,
    })
}

/// HID usage (keyboard page 7) for the Pause key, which has no plain set-1 make.
pub(crate) const HID_USAGE_PAUSE: u8 = 0x48;

/// Set-1 scancodes 0x00..=0x58 → HID keyboard usage; 0 = none. Index = scancode.
#[rustfmt::skip]
const SCAN_TO_HID: [u8; 0x59] = [
    0x00, 0x29, 0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, // ·  Esc 1 2 3 4 5 6
    0x24, 0x25, 0x26, 0x27, 0x2D, 0x2E, 0x2A, 0x2B, // 7 8 9 0 - = Bksp Tab
    0x14, 0x1A, 0x08, 0x15, 0x17, 0x1C, 0x18, 0x0C, // Q W E R T Y U I
    0x12, 0x13, 0x2F, 0x30, 0x28, 0xE0, 0x04, 0x16, // O P [ ] Enter LCtrl A S
    0x07, 0x09, 0x0A, 0x0B, 0x0D, 0x0E, 0x0F, 0x33, // D F G H J K L ;
    0x34, 0x35, 0xE1, 0x31, 0x1D, 0x1B, 0x06, 0x19, // ' ` LShift \ Z X C V
    0x05, 0x11, 0x10, 0x36, 0x37, 0x38, 0xE5, 0x55, // B N M , . / RShift KP*
    0xE2, 0x2C, 0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E, // LAlt Space Caps F1 F2 F3 F4 F5
    0x3F, 0x40, 0x41, 0x42, 0x43, 0x53, 0x47, 0x5F, // F6 F7 F8 F9 F10 NumLock ScrLk KP7
    0x60, 0x61, 0x56, 0x5C, 0x5D, 0x5E, 0x57, 0x59, // KP8 KP9 KP- KP4 KP5 KP6 KP+ KP1
    0x5A, 0x5B, 0x62, 0x63, 0x00, 0x00, 0x64, 0x44, // KP2 KP3 KP0 KP. SysRq · ISO\ F11
    0x45,                                           // F12
];

/// Set-1 scancode (`extended` = E0-prefixed) → HID keyboard usage, the inverse of the table
/// Windows applies to a HID keyboard. `None` for a key a keyboard reports on another usage page
/// (media, power) or that has no make code of its own (SysRq, Pause).
pub(crate) fn scan_to_hid_usage(scan: u16, extended: bool) -> Option<u8> {
    let usage = if extended {
        match scan {
            0x1C => 0x58, // keypad Enter
            0x1D => 0xE4, // right Ctrl
            0x20 => 0x7F, // Mute
            0x2E => 0x81, // Volume Down
            0x30 => 0x80, // Volume Up
            0x35 => 0x54, // keypad /
            0x37 => 0x46, // Print Screen
            0x38 => 0xE6, // right Alt
            0x47 => 0x4A, // Home
            0x48 => 0x52, // Up
            0x49 => 0x4B, // Page Up
            0x4B => 0x50, // Left
            0x4D => 0x4F, // Right
            0x4F => 0x4D, // End
            0x50 => 0x51, // Down
            0x51 => 0x4E, // Page Down
            0x52 => 0x49, // Insert
            0x53 => 0x4C, // Delete
            0x5B => 0xE3, // left Windows
            0x5C => 0xE7, // right Windows
            0x5D => 0x65, // Menu
            _ => 0,
        }
    } else {
        match scan {
            0..=0x58 => SCAN_TO_HID[usize::from(scan)],
            0x59 => 0x67,                              // keypad =
            0x64..=0x6E => 0x68 + (scan - 0x64) as u8, // F13..F23
            0x70 => 0x88,                              // Katakana/Hiragana
            0x73 => 0x87,                              // Ro
            0x76 => 0x73,                              // F24
            0x79 => 0x8A,                              // Henkan
            0x7B => 0x8B,                              // Muhenkan
            0x7D => 0x89,                              // Yen
            0x7E => 0x85,                              // keypad , (Brazil)
            _ => 0,
        }
    };
    (usage != 0).then_some(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pause is 0x13, NumLock is 0x90. Flagging NumLock KEYEVENTF_EXTENDEDKEY invents E0+45.
    #[test]
    fn num_lock_is_not_forced_extended() {
        assert_eq!(vk_to_evdev(VK_PAUSE as u8), Some(119)); // KEY_PAUSE
        assert_eq!(vk_to_evdev(0x90), Some(69)); // VK_NUMLOCK → KEY_NUMLOCK
        assert!(
            !vk_forced_extended(0x90),
            "NumLock make is 0x45 with no E0 prefix"
        );
        assert!(
            !vk_forced_extended(VK_PAUSE),
            "Pause is E1 1D 45, not KEYEVENTF_EXTENDEDKEY"
        );
        assert!(vk_forced_extended(0x26)); // VK_UP
        assert!(vk_forced_extended(0x2D)); // VK_INSERT
        assert!(vk_forced_extended(0x5B)); // VK_LWIN
        assert!(vk_forced_extended(0xA3)); // VK_RCONTROL
        assert!(vk_forced_extended(0xA5)); // VK_RMENU
    }

    #[test]
    fn scancodes_map_to_the_hid_usages_a_keyboard_sends() {
        let plain = |scan| scan_to_hid_usage(scan, false);
        let e0 = |scan| scan_to_hid_usage(scan, true);
        assert_eq!(plain(0x1E), Some(0x04)); // A
        assert_eq!(plain(0x2C), Some(0x1D)); // Z
        assert_eq!(plain(0x02), Some(0x1E)); // 1
        assert_eq!(plain(0x0B), Some(0x27)); // 0
        assert_eq!(plain(0x1C), Some(0x28)); // Enter
        assert_eq!(e0(0x1C), Some(0x58)); // keypad Enter
        assert_eq!(plain(0x1D), Some(0xE0)); // left Ctrl
        assert_eq!(e0(0x1D), Some(0xE4)); // right Ctrl
        assert_eq!(plain(0x3B), Some(0x3A)); // F1
        assert_eq!(plain(0x58), Some(0x45)); // F12
        assert_eq!(plain(0x56), Some(0x64)); // ISO key left of Z
        assert_eq!(e0(0x48), Some(0x52)); // Up
        assert_eq!(plain(0x48), Some(0x60)); // keypad 8
        assert_eq!(plain(0x45), Some(0x53)); // NumLock
        assert_eq!(plain(0x54), None); // SysRq
        assert_eq!(e0(0x22), None); // Play/Pause: consumer page
                                    // No two keys share a usage.
        let mut seen = std::collections::HashSet::new();
        for scan in 0..0x80u16 {
            for ext in [false, true] {
                if let Some(u) = scan_to_hid_usage(scan, ext) {
                    assert!(u <= 0xE7, "{scan:#x}");
                    assert!(seen.insert(u), "usage {u:#x} twice");
                }
            }
        }
    }

    /// Every positional VK below F13 has a set-1 make equal to its evdev code; each one must
    /// reach a HID usage.
    #[test]
    fn every_positional_key_has_a_hid_usage() {
        for vk in 0..=255u8 {
            let Some(code) = vk_to_evdev(vk) else {
                continue;
            };
            if code <= 0x58 && !matches!(code, 0x54 | 0x55) {
                assert!(scan_to_hid_usage(code, false).is_some(), "vk {vk:#x}");
            }
        }
    }
}
