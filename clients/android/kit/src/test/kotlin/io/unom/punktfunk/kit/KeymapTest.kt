package io.unom.punktfunk.kit

import android.view.KeyEvent
import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of the positional scancode table (`Keymap.evdevToVk`) — no Android runtime types
 * (the `KeyEvent` constants in the keycode table are compile-time-inlined ints). Run:
 * `./gradlew :kit:testDebugUnitTest`.
 */
class KeymapTest {
    /**
     * The German-scramble regression pins: the physical keys a QWERTZ board labels Z/Y/ö/ü/ä/ß
     * must leave this client as their US-position VKs, regardless of the user-selected physical
     * keyboard layout (which remaps `keyCode`, not `scanCode`).
     */
    @Test
    fun positionalPinsForTheQwertzScramble() {
        assertEquals(0x59, Keymap.evdevToVk(21)) // KEY_Y (QWERTZ: Z key) → VK_Y
        assertEquals(0x5A, Keymap.evdevToVk(44)) // KEY_Z (QWERTZ: Y key) → VK_Z
        assertEquals(0xBA, Keymap.evdevToVk(39)) // KEY_SEMICOLON (QWERTZ: ö) → VK_OEM_1
        assertEquals(0xDB, Keymap.evdevToVk(26)) // KEY_LEFTBRACE (QWERTZ: ü) → VK_OEM_4
        assertEquals(0xDE, Keymap.evdevToVk(40)) // KEY_APOSTROPHE (QWERTZ: ä) → VK_OEM_7
        assertEquals(0xBD, Keymap.evdevToVk(12)) // KEY_MINUS (QWERTZ: ß) → VK_OEM_MINUS
    }

    /**
     * Exactly the 48 typing-area keys are covered (10 digits + 26 letters + 12 OEM) with unique
     * VKs; everything else (nav, F-row, modifiers, gamepad buttons at 0x100+) falls through to
     * the keycode table.
     */
    @Test
    fun tableCoversTheTypingAreaBijectively() {
        val mapped = (0..0x200).mapNotNull { sc ->
            Keymap.evdevToVk(sc).takeIf { it != 0 }?.let { sc to it }
        }
        assertEquals(48, mapped.size)
        assertEquals(48, mapped.map { it.second }.toSet().size)
        assertEquals(0, Keymap.evdevToVk(1)) // KEY_ESC — layout-invariant, keycode path
        assertEquals(0, Keymap.evdevToVk(59)) // KEY_F1
        assertEquals(0, Keymap.evdevToVk(304)) // BTN_SOUTH — gamepad, never a typing key
    }

    /**
     * `crates/core/punktfunk-core/testdata/evdev-vk-vectors.json`, which core's `evdev_to_vk` writes:
     * every scancode this table maps agrees with it, and every layout-variant key (digits,
     * letters, OEM punctuation) core maps is covered here.
     */
    @Test
    fun matchesTheCoreVectors() {
        val file = File("../../../crates/core/punktfunk-core/testdata/evdev-vk-vectors.json")
        assertTrue("the vector file must be reachable at ${file.absolutePath}", file.isFile)
        val cases = JSONObject(file.readText()).getJSONArray("cases")
        val oem = (0xBA..0xC0) + (0xDB..0xDE) + 0xE2
        var typing = 0
        for (i in 0 until cases.length()) {
            val case = cases.getJSONObject(i)
            val scan = case.getInt("evdev")
            val vk = if (case.isNull("vk")) 0 else case.getInt("vk")
            val mine = Keymap.evdevToVk(scan)
            if (vk in 0x30..0x39 || vk in 0x41..0x5A || vk in oem) {
                typing++
                assertEquals("evdev $scan", vk, mine)
            } else if (mine != 0) {
                assertEquals("evdev $scan", vk, mine)
            }
        }
        assertEquals(48, typing)
    }

    /** Korean and JIS IME keys leave as the VKs the hosts map, under Android's odd names. */
    @Test
    fun imeKeysReachTheWire() {
        assertEquals(0x15, Keymap.toVk(KeyEvent.KEYCODE_KANA)) // 한/영
        assertEquals(0x19, Keymap.toVk(KeyEvent.KEYCODE_EISU)) // 한자
        assertEquals(0x1C, Keymap.toVk(KeyEvent.KEYCODE_HENKAN))
        assertEquals(0x1D, Keymap.toVk(KeyEvent.KEYCODE_MUHENKAN))
        assertEquals(0xF2, Keymap.toVk(KeyEvent.KEYCODE_KATAKANA_HIRAGANA))
        assertEquals(0xF3, Keymap.toVk(KeyEvent.KEYCODE_ZENKAKU_HANKAKU))
        assertEquals(0xC1, Keymap.toVk(KeyEvent.KEYCODE_RO)) // JIS ろ, ABNT2 /?
        assertEquals(0xC2, Keymap.toVk(KeyEvent.KEYCODE_NUMPAD_COMMA))
        assertEquals(0xE1, Keymap.toVk(KeyEvent.KEYCODE_YEN))
        assertEquals(0, Keymap.toVk(KeyEvent.KEYCODE_LANGUAGE_SWITCH)) // Android keeps it
    }

    /** Alt+` is Alt+Tab: the grave goes out as Tab from its Alt-held press to its release. */
    @Test
    fun altGraveStaysTabUntilReleased() {
        var tab = Keymap.altTabAlias(down = true, repeat = false, altOnly = true, wasTab = false)
        assertTrue(tab)
        tab = Keymap.altTabAlias(down = true, repeat = true, altOnly = false, wasTab = tab)
        assertTrue("Alt let go mid-repeat", tab)
        tab = Keymap.altTabAlias(down = false, repeat = false, altOnly = false, wasTab = tab)
        assertTrue("the release matches the press", tab)
        assertFalse("a plain grave", Keymap.altTabAlias(down = true, repeat = false, altOnly = false, wasTab = tab))
    }
}
