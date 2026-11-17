package io.unom.punktfunk.kit

import android.view.KeyEvent

/**
 * Hardware key → Windows Virtual-Key code (the punktfunk wire contract: **US-positional** — we
 * forward the physical key, not the typed character; the host maps VK → evdev via
 * `vk_to_evdev`). The Android analogue of the Linux client's evdev→VK table
 * (`punktfunk_core::input::evdev_to_vk`) and the Apple client's `hidToVK`.
 *
 * Prefer [toVk] with the full [KeyEvent]: it reads the raw evdev scancode first, because
 * `KeyEvent.keyCode` is only positional under the stock US key layout — a user-selected physical
 * keyboard layout (Settings → Physical keyboard) remaps keycodes semantically (AOSP's German .kcm
 * carries `map key 21 Z` / `map key 44 Y`), which would apply the layout twice: once here, once on
 * the host (the y↔z / ü-on-ö scramble). Unmapped keys → 0 (the Rust side drops them). Extend this
 * alongside `pf-inject/src/inject/keymap.rs::vk_to_evdev` (emit only VKs the host knows).
 */
object Keymap {
    /**
     * Positional wire VK for a hardware key event: the evdev scancode table first (immune to the
     * selected physical-keyboard layout), falling back to the keycode table for events without a
     * scancode (soft keyboards, synthetic events) and for everything outside the typing area
     * (layout-invariant there, incl. gamepad buttons whose scancodes lie outside the table).
     */
    fun toVk(event: KeyEvent): Int {
        val positional = evdevToVk(event.scanCode)
        return if (positional != 0) positional else toVk(event.keyCode)
    }

    /**
     * Linux evdev keycode (`KeyEvent.scanCode`) → US-positional VK for the layout-**variant**
     * typing area: the 48 keys of `punktfunk_core::input::evdev_to_vk` that move with the layout,
     * held to it by `evdev-vk-vectors.json`. Everything else → 0 (the keycode path is already
     * positional for those).
     */
    fun evdevToVk(scan: Int): Int = when (scan) {
        in 2..10 -> 0x31 + (scan - 2) // KEY_1..KEY_9
        11 -> 0x30 // KEY_0
        12 -> 0xBD // KEY_MINUS       -_  VK_OEM_MINUS (DE: ß)
        13 -> 0xBB // KEY_EQUAL       =+  VK_OEM_PLUS
        16 -> 0x51 // Q
        17 -> 0x57 // W
        18 -> 0x45 // E
        19 -> 0x52 // R
        20 -> 0x54 // T
        21 -> 0x59 // KEY_Y — US-Y position (QWERTZ: the Z key)
        22 -> 0x55 // U
        23 -> 0x49 // I
        24 -> 0x4F // O
        25 -> 0x50 // P
        26 -> 0xDB // KEY_LEFTBRACE   [{  VK_OEM_4 (DE: ü)
        27 -> 0xDD // KEY_RIGHTBRACE  ]}  VK_OEM_6
        30 -> 0x41 // A
        31 -> 0x53 // S
        32 -> 0x44 // D
        33 -> 0x46 // F
        34 -> 0x47 // G
        35 -> 0x48 // H
        36 -> 0x4A // J
        37 -> 0x4B // K
        38 -> 0x4C // L
        39 -> 0xBA // KEY_SEMICOLON   ;:  VK_OEM_1 (DE: ö)
        40 -> 0xDE // KEY_APOSTROPHE  '"  VK_OEM_7 (DE: ä)
        41 -> 0xC0 // KEY_GRAVE       `~  VK_OEM_3 (DE: ^)
        43 -> 0xDC // KEY_BACKSLASH   \|  VK_OEM_5
        44 -> 0x5A // KEY_Z — US-Z position (QWERTZ: the Y key)
        45 -> 0x58 // X
        46 -> 0x43 // C
        47 -> 0x56 // V
        48 -> 0x42 // B
        49 -> 0x4E // N
        50 -> 0x4D // M
        51 -> 0xBC // KEY_COMMA       ,<  VK_OEM_COMMA
        52 -> 0xBE // KEY_DOT         .>  VK_OEM_PERIOD
        53 -> 0xBF // KEY_SLASH       /?  VK_OEM_2
        86 -> 0xE2 // KEY_102ND       <>| VK_OEM_102 (ISO)
        else -> 0
    }

    fun toVk(keyCode: Int): Int = when (keyCode) {
        in KeyEvent.KEYCODE_A..KeyEvent.KEYCODE_Z -> 0x41 + (keyCode - KeyEvent.KEYCODE_A) // A–Z
        in KeyEvent.KEYCODE_0..KeyEvent.KEYCODE_9 -> 0x30 + (keyCode - KeyEvent.KEYCODE_0) // 0–9 row
        in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 -> 0x70 + (keyCode - KeyEvent.KEYCODE_F1) // F1–F12
        in KeyEvent.KEYCODE_NUMPAD_0..KeyEvent.KEYCODE_NUMPAD_9 ->
            0x60 + (keyCode - KeyEvent.KEYCODE_NUMPAD_0) // numpad 0–9

        // Whitespace / editing
        KeyEvent.KEYCODE_DEL -> 0x08 // Backspace (Android KEYCODE_DEL == backspace)
        KeyEvent.KEYCODE_TAB -> 0x09
        KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> 0x0D
        KeyEvent.KEYCODE_ESCAPE -> 0x1B
        KeyEvent.KEYCODE_SPACE -> 0x20
        KeyEvent.KEYCODE_CAPS_LOCK -> 0x14
        KeyEvent.KEYCODE_BREAK -> 0x13 // Pause
        KeyEvent.KEYCODE_SYSRQ -> 0x2C // PrintScreen
        KeyEvent.KEYCODE_INSERT -> 0x2D
        KeyEvent.KEYCODE_FORWARD_DEL -> 0x2E // Delete (forward)
        KeyEvent.KEYCODE_NUM_LOCK -> 0x90
        KeyEvent.KEYCODE_SCROLL_LOCK -> 0x91

        // Navigation
        KeyEvent.KEYCODE_PAGE_UP -> 0x21
        KeyEvent.KEYCODE_PAGE_DOWN -> 0x22
        KeyEvent.KEYCODE_MOVE_END -> 0x23
        KeyEvent.KEYCODE_MOVE_HOME -> 0x24
        KeyEvent.KEYCODE_DPAD_LEFT -> 0x25
        KeyEvent.KEYCODE_DPAD_UP -> 0x26
        KeyEvent.KEYCODE_DPAD_RIGHT -> 0x27
        KeyEvent.KEYCODE_DPAD_DOWN -> 0x28
        // TV-remote SELECT = Enter (a gamepad's press routes via SOURCE_GAMEPAD before this).
        KeyEvent.KEYCODE_DPAD_CENTER -> 0x0D

        // Consumer/media keys — forwarded to the host while streaming (volume stays local:
        // MainActivity's SYSTEM_KEYS wins before the map is consulted).
        KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE,
        KeyEvent.KEYCODE_MEDIA_PLAY,
        KeyEvent.KEYCODE_MEDIA_PAUSE -> 0xB3 // VK_MEDIA_PLAY_PAUSE
        KeyEvent.KEYCODE_MEDIA_NEXT -> 0xB0 // VK_MEDIA_NEXT_TRACK
        KeyEvent.KEYCODE_MEDIA_PREVIOUS -> 0xB1 // VK_MEDIA_PREV_TRACK
        KeyEvent.KEYCODE_MEDIA_STOP -> 0xB2 // VK_MEDIA_STOP

        // Modifiers (L/R-specific VKs; the host folds the generic ones onto the left variant)
        KeyEvent.KEYCODE_SHIFT_LEFT -> 0xA0
        KeyEvent.KEYCODE_SHIFT_RIGHT -> 0xA1
        KeyEvent.KEYCODE_CTRL_LEFT -> 0xA2
        KeyEvent.KEYCODE_CTRL_RIGHT -> 0xA3
        KeyEvent.KEYCODE_ALT_LEFT -> 0xA4
        KeyEvent.KEYCODE_ALT_RIGHT -> 0xA5 // AltGr
        KeyEvent.KEYCODE_META_LEFT -> 0x5B // Super/Win
        KeyEvent.KEYCODE_META_RIGHT -> 0x5C
        KeyEvent.KEYCODE_MENU -> 0x5D // Application

        // Numpad operators
        KeyEvent.KEYCODE_NUMPAD_MULTIPLY -> 0x6A
        KeyEvent.KEYCODE_NUMPAD_ADD -> 0x6B
        KeyEvent.KEYCODE_NUMPAD_SUBTRACT -> 0x6D
        KeyEvent.KEYCODE_NUMPAD_DOT -> 0x6E
        KeyEvent.KEYCODE_NUMPAD_DIVIDE -> 0x6F

        // OEM punctuation (US-layout positional)
        KeyEvent.KEYCODE_SEMICOLON -> 0xBA
        KeyEvent.KEYCODE_EQUALS -> 0xBB
        KeyEvent.KEYCODE_COMMA -> 0xBC
        KeyEvent.KEYCODE_MINUS -> 0xBD
        KeyEvent.KEYCODE_PERIOD -> 0xBE
        KeyEvent.KEYCODE_SLASH -> 0xBF
        KeyEvent.KEYCODE_GRAVE -> 0xC0
        KeyEvent.KEYCODE_LEFT_BRACKET -> 0xDB
        KeyEvent.KEYCODE_BACKSLASH -> 0xDC
        KeyEvent.KEYCODE_RIGHT_BRACKET -> 0xDD
        KeyEvent.KEYCODE_APOSTROPHE -> 0xDE

        // IME keys. Android's Generic.kl names a Korean board's 한/영 (KEY_HANGEUL) KANA and its
        // 한자 (KEY_HANJA) EISU; Windows gives Korean and Japanese the same two VKs anyway.
        KeyEvent.KEYCODE_KANA -> 0x15 // VK_HANGUL / VK_KANA
        KeyEvent.KEYCODE_EISU -> 0x19 // VK_HANJA / VK_KANJI
        KeyEvent.KEYCODE_HENKAN -> 0x1C // VK_CONVERT
        KeyEvent.KEYCODE_MUHENKAN -> 0x1D // VK_NONCONVERT
        KeyEvent.KEYCODE_KATAKANA_HIRAGANA -> 0xF2 // VK_DBE_HIRAGANA
        KeyEvent.KEYCODE_ZENKAKU_HANKAKU -> 0xF3 // VK_DBE_SBCSCHAR
        // The keys a US board lacks, under the ABNT/AX labels the hosts map by scancode.
        KeyEvent.KEYCODE_RO -> 0xC1 // JIS ろ, ABNT2 /?
        KeyEvent.KEYCODE_NUMPAD_COMMA -> 0xC2 // ABNT2 keypad .
        KeyEvent.KEYCODE_YEN -> 0xE1 // JIS ¥

        else -> 0 // unmapped → Rust drops it
    }

    /**
     * Android keeps Alt+Tab (its app switcher) and every Meta chord for itself, so Alt+` stands
     * in: the grave key goes out as Tab while Alt alone holds it down, and a Shift with it walks
     * the switcher backwards like the real key. Answers for one grave event; [wasTab] is the
     * answer for the previous one, carried through repeats and the release so a Tab never sticks.
     */
    fun altTabAlias(down: Boolean, repeat: Boolean, altOnly: Boolean, wasTab: Boolean): Boolean =
        if (down && !repeat) altOnly else wasTab
}
