// InputCapture's static keymap tables: HID usage → Windows VK (the GCKeyboard path off macOS,
// and UIKit's shortcut recorder) and, on macOS, NSEvent.keyCode → Windows VK (the NSEvent path).

import PunktfunkShared

extension InputCapture {
    /// The Windows VK of a HID usage (a UIKit `UIKey.keyCode` raw value), for the Shortcuts
    /// page's recorder.
    public nonisolated static func vk(hidUsage: Int) -> UInt32? { hidToVK[hidUsage] }

    #if os(macOS)
    /// The Windows VK of an `NSEvent.keyCode`, for the Shortcuts page's recorder.
    public nonisolated static func vk(keyCode: UInt16) -> UInt32? { keyCodeToVK[keyCode] }
    #endif

    /// Remap one modifier VK for the active location-based [`ModifierLayout`] just before it goes on
    /// the wire. `.mac` (and every non-modifier VK) is the identity. `.windows` swaps the Alt vs
    /// Super/Windows ROLE between the Option and Command keys while KEEPING the side, so a Windows
    /// user's `⌘ = Alt, ⌥ = Win` muscle memory lands correctly:
    ///   L Command 0x5B ↔ L Alt 0xA4   ·   R Command 0x5C ↔ R Alt 0xA5.
    /// It's applied ONLY at the send boundary (`InputCapture.emitKey`) — all press/release
    /// bookkeeping stays on the physical VK, so modifier direction tracking and the client-local
    /// ⌘-based shortcuts are untouched. The swap is its own inverse: a key that went down remapped
    /// goes up remapped, so the host never sees a stuck modifier.
    nonisolated static func applyModifierLayout(_ vk: UInt32, _ layout: ModifierLayout) -> UInt32 {
        guard layout == .windows else { return vk }
        switch vk {
        case 0x5B: return 0xA4 // L Command → L Alt (VK_LWIN → VK_LMENU)
        case 0x5C: return 0xA5 // R Command → R Alt (VK_RWIN → VK_RMENU)
        case 0xA4: return 0x5B // L Option  → L Win (VK_LMENU → VK_LWIN)
        case 0xA5: return 0x5C // R Option  → R Win (VK_RMENU → VK_RWIN)
        default: return vk
        }
    }
    /// HID usage (GCKeyCode raw) → Windows VK (the host maps VK → evdev; every VK emitted
    /// here exists in punktfunk-host/src/inject.rs::vk_to_evdev — extend the two together).
    nonisolated static let hidToVK: [Int: UInt32] = {
        var m: [Int: UInt32] = [:]
        // a–z: HID 0x04..0x1D → VK 'A'..'Z'.
        for i in 0..<26 { m[0x04 + i] = UInt32(0x41 + i) }
        // 1–9: HID 0x1E..0x26 → VK '1'..'9'; then 0: HID 0x27 → VK '0' (set separately —
        // the '0' key sits AFTER '9' in HID but its VK 0x30 sits BEFORE '1' (0x31)).
        for i in 0..<9 { m[0x1E + i] = UInt32(0x31 + i) }
        m[0x27] = 0x30
        m[0x28] = 0x0D // return
        m[0x29] = 0x1B // escape
        m[0x2A] = 0x08 // backspace
        m[0x2B] = 0x09 // tab
        m[0x2C] = 0x20 // space
        m[0x2D] = 0xBD; m[0x2E] = 0xBB // - =
        m[0x2F] = 0xDB; m[0x30] = 0xDD; m[0x31] = 0xDC // [ ] backslash
        m[0x33] = 0xBA; m[0x34] = 0xDE; m[0x35] = 0xC0 // ; ' `
        m[0x36] = 0xBC; m[0x37] = 0xBE; m[0x38] = 0xBF // , . /
        m[0x39] = 0x14 // caps lock
        // F1..F12: HID 0x3A..0x45 → VK 0x70..0x7B.
        for i in 0..<12 { m[0x3A + i] = UInt32(0x70 + i) }
        m[0x46] = 0x2C; m[0x47] = 0x91; m[0x48] = 0x13 // printscreen scrolllock pause
        m[0x4F] = 0x27; m[0x50] = 0x25; m[0x51] = 0x28; m[0x52] = 0x26 // arrows R L D U
        m[0x49] = 0x2D; m[0x4A] = 0x24; m[0x4B] = 0x21 // insert home pageup
        m[0x4C] = 0x2E; m[0x4D] = 0x23; m[0x4E] = 0x22 // delete end pagedown
        // Keypad: NumLock, / * - +, Enter, 1..9, 0, decimal. KP Enter goes as
        // VK_SEPARATOR (0x6C) — this host maps it to KEY_KPENTER (Windows itself would
        // send VK_RETURN+extended, which vk_to_evdev can't distinguish).
        m[0x53] = 0x90
        m[0x54] = 0x6F; m[0x55] = 0x6A; m[0x56] = 0x6D; m[0x57] = 0x6B
        m[0x58] = 0x6C
        for i in 0..<9 { m[0x59 + i] = UInt32(0x61 + i) }
        m[0x62] = 0x60; m[0x63] = 0x6E
        m[0x64] = 0xE2 // ISO 102nd key (<> next to left shift on ISO layouts)
        m[0x65] = 0x5D // menu/application
        // IME keys: Korean 한/영 and 한자 (also JIS かな/英数), JIS 変換 無変換 カタカナ/ひらがな.
        m[0x90] = 0x15; m[0x91] = 0x19
        m[0x8A] = 0x1C; m[0x8B] = 0x1D; m[0x88] = 0xF2
        // The keys a US board lacks: JIS ろ (also ABNT2 /?), ABNT2 keypad ., JIS ¥.
        m[0x87] = 0xC1; m[0x85] = 0xC2; m[0x89] = 0xE1
        m[0xE0] = 0xA2; m[0xE1] = 0xA0; m[0xE2] = 0xA4; m[0xE3] = 0x5B // Lctrl Lshift Lalt Lcmd
        m[0xE4] = 0xA3; m[0xE5] = 0xA1; m[0xE6] = 0xA5; m[0xE7] = 0x5C // Rctrl Rshift Ralt Rcmd
        return m
    }()

    #if os(macOS)
    /// NSEvent.keyCode (Carbon virtual keycode, kVK_*) → Windows VK. The macOS NSEvent key
    /// path is keyed by keyCode (a layout-independent hardware position), NOT by HID usage,
    /// so it needs its own table — but it emits the EXACT SAME Windows VK integers `hidToVK`
    /// already produces for each physical key (A→0x41, Return→0x0D, KeypadEnter→0x6C, …), so
    /// the host's vk_to_evdev (inject.rs) accepts both with zero change. Modifier keys come
    /// via flagsChanged (handleFlagsChanged), not keyDown, so they're absent here. Keys with
    /// no host evdev arm (F13–F20, KeypadEquals, the Fn key) are omitted → nil → swallowed.
    nonisolated static let keyCodeToVK: [UInt16: UInt32] = {
        var m: [UInt16: UInt32] = [:]
        // Letters — kVK_ANSI_A..Z (scattered keycodes) → VK 'A'..'Z'.
        m[0x00] = 0x41; m[0x01] = 0x53; m[0x02] = 0x44; m[0x03] = 0x46 // A S D F
        m[0x04] = 0x48; m[0x05] = 0x47; m[0x06] = 0x5A; m[0x07] = 0x58 // H G Z X
        m[0x08] = 0x43; m[0x09] = 0x56; m[0x0B] = 0x42; m[0x0C] = 0x51 // C V B Q
        m[0x0D] = 0x57; m[0x0E] = 0x45; m[0x0F] = 0x52; m[0x10] = 0x59 // W E R Y
        m[0x11] = 0x54; m[0x1F] = 0x4F; m[0x20] = 0x55; m[0x22] = 0x49 // T O U I
        m[0x23] = 0x50; m[0x25] = 0x4C; m[0x26] = 0x4A; m[0x28] = 0x4B // P L J K
        m[0x2D] = 0x4E; m[0x2E] = 0x4D // N M
        // Digit row — kVK_ANSI_1..0 (scattered) → VK '1'..'9','0'.
        m[0x12] = 0x31; m[0x13] = 0x32; m[0x14] = 0x33; m[0x15] = 0x34 // 1 2 3 4
        m[0x16] = 0x36; m[0x17] = 0x35; m[0x19] = 0x39; m[0x1A] = 0x37 // 6 5 9 7
        m[0x1C] = 0x38; m[0x1D] = 0x30 // 8 0
        // Whitespace / control.
        m[0x24] = 0x0D // return
        m[0x30] = 0x09 // tab
        m[0x31] = 0x20 // space
        m[0x33] = 0x08 // delete (backspace)
        m[0x35] = 0x1B // escape
        m[0x75] = 0x2E // forward delete (VK_DELETE)
        m[0x39] = 0x14 // caps lock
        // Punctuation (US ANSI) + ISO 102nd key.
        m[0x1B] = 0xBD; m[0x18] = 0xBB // - = (OEM_MINUS OEM_PLUS)
        m[0x21] = 0xDB; m[0x1E] = 0xDD; m[0x2A] = 0xDC // [ ] backslash (OEM_4 6 5)
        m[0x29] = 0xBA; m[0x27] = 0xDE; m[0x32] = 0xC0 // ; ' ` (OEM_1 7 3)
        m[0x2B] = 0xBC; m[0x2F] = 0xBE; m[0x2C] = 0xBF // , . / (OEM_COMMA PERIOD 2)
        m[0x0A] = 0xE2 // ISO 102nd key (<> next to left shift; OEM_102)
        // JIS ¥ ろ keypad-comma かな 英数: the VKs `hidToVK` sends for HID 0x89 0x87 0x85 0x90 0x91.
        m[0x5D] = 0xE1; m[0x5E] = 0xC1; m[0x5F] = 0xC2; m[0x68] = 0x15; m[0x66] = 0x19
        // Function keys F1..F12 (scattered) → VK 0x70..0x7B. F13+ omitted (no host arm).
        m[0x7A] = 0x70; m[0x78] = 0x71; m[0x63] = 0x72; m[0x76] = 0x73 // F1 F2 F3 F4
        m[0x60] = 0x74; m[0x61] = 0x75; m[0x62] = 0x76; m[0x64] = 0x77 // F5 F6 F7 F8
        m[0x65] = 0x78; m[0x6D] = 0x79; m[0x67] = 0x7A; m[0x6F] = 0x7B // F9 F10 F11 F12
        // Arrows.
        m[0x7B] = 0x25; m[0x7C] = 0x27; m[0x7D] = 0x28; m[0x7E] = 0x26 // left right down up
        // Nav cluster (Apple keycodes; Help sits where Insert is).
        m[0x72] = 0x2D; m[0x73] = 0x24; m[0x74] = 0x21 // insert home pageup
        m[0x77] = 0x23; m[0x79] = 0x22 // end pagedown (forward-delete handled above)
        // Keypad — kVK_ANSI_Keypad0..9 (scattered) → VK_NUMPAD0..9, plus the operators.
        m[0x52] = 0x60; m[0x53] = 0x61; m[0x54] = 0x62; m[0x55] = 0x63 // KP0 KP1 KP2 KP3
        m[0x56] = 0x64; m[0x57] = 0x65; m[0x58] = 0x66; m[0x59] = 0x67 // KP4 KP5 KP6 KP7
        m[0x5B] = 0x68; m[0x5C] = 0x69 // KP8 KP9
        m[0x41] = 0x6E; m[0x43] = 0x6A; m[0x45] = 0x6B // KP decimal multiply plus
        m[0x4E] = 0x6D; m[0x4B] = 0x6F // KP minus divide
        m[0x4C] = 0x6C // KP enter → VK_SEPARATOR (host maps to KEY_KPENTER, matching hidToVK)
        m[0x47] = 0x90 // KP clear sits where NumLock is → VK_NUMLOCK. (KP equals 0x51 dropped.)
        return m
    }()

    /// NSEvent.keyCode of each modifier key (kVK_Shift & co. — modifiers arrive only as
    /// flagsChanged) → its Windows VK plus the `NSEvent.modifierFlags` bits that describe
    /// it: `classMask` is the device-INDEPENDENT NX_*MASK for the modifier class,
    /// `deviceBit`/`siblingBit` the device-dependent bits (LOW 16 bits, NX_DEVICE*KEYMASK
    /// in IOLLEvent.h) for this key and its opposite-side twin. Consumed by
    /// `resolveModifier`, which explains why both kinds of bit are needed.
    nonisolated static let modifierBits:
        [UInt16: (vk: UInt32, classMask: UInt, deviceBit: UInt, siblingBit: UInt)] = [
            56: (0xA0, 0x2_0000, 0x2, 0x4), // left shift → VK_LSHIFT
            60: (0xA1, 0x2_0000, 0x4, 0x2), // right shift → VK_RSHIFT
            59: (0xA2, 0x4_0000, 0x1, 0x2000), // left control → VK_LCONTROL
            62: (0xA3, 0x4_0000, 0x2000, 0x1), // right control → VK_RCONTROL
            58: (0xA4, 0x8_0000, 0x20, 0x40), // left option → VK_LMENU
            61: (0xA5, 0x8_0000, 0x40, 0x20), // right option → VK_RMENU
            55: (0x5B, 0x10_0000, 0x8, 0x10), // left command → VK_LWIN
            54: (0x5C, 0x10_0000, 0x10, 0x8), // right command → VK_RWIN
        ]
    #endif
}

#if os(iOS) || os(visionOS)
/// US-layout character → Windows VK for the on-screen keyboard (`StreamLayerUIView`'s
/// UIKeyInput). Unlike every other key source, `insertText` delivers CHARACTERS, not key
/// positions, so this is the inverse of a US layout: `shift` means "wrap in VK_LSHIFT so the
/// host types the shifted symbol". Same contract as `hidToVK`: emit only VKs the host's
/// vk_to_evdev knows; anything unmapped is dropped by the caller.
enum SoftKeyMap {
    static func vk(for ch: Character) -> (vk: UInt32, shift: Bool)? {
        guard let ascii = ch.asciiValue else { return nil }
        switch ascii {
        case UInt8(ascii: "a")...UInt8(ascii: "z"): return (UInt32(ascii) - 0x20, false)
        case UInt8(ascii: "A")...UInt8(ascii: "Z"): return (UInt32(ascii), true)
        case UInt8(ascii: "0")...UInt8(ascii: "9"): return (UInt32(ascii), false)
        case 0x0A, 0x0D: return (0x0D, false) // return
        case 0x09: return (0x09, false) // tab
        case 0x20: return (0x20, false) // space
        default: return symbols[ch]
        }
    }

    /// US punctuation, plain and shifted, on the OEM VKs (mirrors `hidToVK`'s OEM block) plus
    /// the shifted digit row.
    private static let symbols: [Character: (vk: UInt32, shift: Bool)] = [
        "-": (0xBD, false), "_": (0xBD, true),
        "=": (0xBB, false), "+": (0xBB, true),
        "[": (0xDB, false), "{": (0xDB, true),
        "]": (0xDD, false), "}": (0xDD, true),
        "\\": (0xDC, false), "|": (0xDC, true),
        ";": (0xBA, false), ":": (0xBA, true),
        "'": (0xDE, false), "\"": (0xDE, true),
        "`": (0xC0, false), "~": (0xC0, true),
        ",": (0xBC, false), "<": (0xBC, true),
        ".": (0xBE, false), ">": (0xBE, true),
        "/": (0xBF, false), "?": (0xBF, true),
        "!": (0x31, true), "@": (0x32, true), "#": (0x33, true), "$": (0x34, true),
        "%": (0x35, true), "^": (0x36, true), "&": (0x37, true), "*": (0x38, true),
        "(": (0x39, true), ")": (0x30, true),
    ]
}
#endif
