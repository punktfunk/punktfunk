package io.unom.punktfunk.kit

import android.view.KeyEvent
import android.view.MotionEvent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of [Gamepad.PadButtons.correct] — the scancode resolution for controllers Android
 * has no key layout for. Only `KeyEvent`'s compile-time-inlined keycode constants are involved, so
 * no Android runtime is needed. Run: `./gradlew :kit:testDebugUnitTest`.
 *
 * A pad with no key layout falls back to `Generic.kl`, which names keycodes by scancode POSITION.
 * A pad with no kernel driver numbers its HID buttons 1..n straight through in its own report
 * order, so every keycode after the first divergence belongs to a different button; `correct`
 * resolves the button from the scancode instead.
 *
 * An assertion expects the physical button; a trailing `delivered as` comment names the keycode
 * `Generic.kl` gives it.
 */
class PadButtonsTest {

    private fun sony(scan: Int) =
        Gamepad.PadButtons.GENERIC_SONY.correct(scan, Gamepad.genericKeyCode(scan))

    private fun xbox(scan: Int) =
        Gamepad.PadButtons.GENERIC_XBOX.correct(scan, Gamepad.genericKeyCode(scan))

    /**
     * A driverless DualSense's L2 sits at scancode `0x136`, which `Generic.kl` calls BUTTON_L1,
     * and its L1 (`0x134`) arrives as BUTTON_Y. Each is put back on its own button.
     */
    @Test
    fun `a DualSense's shoulders stop being each other's buttons`() {
        assertEquals(KeyEvent.KEYCODE_BUTTON_L1, sony(0x134)) // L1, delivered as BUTTON_Y
        assertEquals(KeyEvent.KEYCODE_BUTTON_R1, sony(0x135)) // R1, delivered as BUTTON_Z
        assertEquals(KeyEvent.KEYCODE_BUTTON_L2, sony(0x136)) // L2, delivered as BUTTON_L1
        assertEquals(KeyEvent.KEYCODE_BUTTON_R2, sony(0x137)) // R2, delivered as BUTTON_R1
    }

    /** ✕ is the bottom button — the one A means everywhere else — and □ is the left one. */
    @Test
    fun `a DualSense's face buttons land on their Xbox positions`() {
        assertEquals(KeyEvent.KEYCODE_BUTTON_X, sony(0x130)) // □
        assertEquals(KeyEvent.KEYCODE_BUTTON_A, sony(0x131)) // ✕
        assertEquals(KeyEvent.KEYCODE_BUTTON_B, sony(0x132)) // ○
        assertEquals(KeyEvent.KEYCODE_BUTTON_Y, sony(0x133)) // △
    }

    /**
     * Create/Options/L3/R3/PS. Create is Select, which the exit, mic and stats chords are built
     * on.
     */
    @Test
    fun `a DualSense's menu buttons and stick clicks are themselves`() {
        assertEquals(KeyEvent.KEYCODE_BUTTON_SELECT, sony(0x138)) // Create
        assertEquals(KeyEvent.KEYCODE_BUTTON_START, sony(0x139)) // Options
        assertEquals(KeyEvent.KEYCODE_BUTTON_THUMBL, sony(0x13a)) // L3
        assertEquals(KeyEvent.KEYCODE_BUTTON_THUMBR, sony(0x13b)) // R3
        assertEquals(KeyEvent.KEYCODE_BUTTON_MODE, sony(0x13c)) // PS
    }

    /**
     * The touchpad click and the mute button reach the wire, on the two bits that exist for them.
     * Android has no keycode for either, so [Gamepad.PadButtons.GENERIC_SONY] borrows BUTTON_15
     * and BUTTON_16 to carry them into [Gamepad.buttonBit] — the keycode is an implementation
     * detail of that hop, the BIT is the contract, so both halves are pinned here.
     */
    @Test
    fun `a DualSense's touchpad and mute reach their wire buttons`() {
        assertEquals(KeyEvent.KEYCODE_BUTTON_15, sony(0x13d))
        assertEquals(KeyEvent.KEYCODE_BUTTON_16, sony(0x13e))
        assertEquals(Gamepad.BTN_TOUCHPAD, Gamepad.buttonBit(sony(0x13d)))
        assertEquals(Gamepad.BTN_MISC1, Gamepad.buttonBit(sony(0x13e)))
    }

    /**
     * The touchpad/mute rows live inside GENERIC_SONY, not above `padMap(dev)`.
     *
     * `0x13d`/`0x13e` are `BTN_THUMBL`/`BTN_THUMBR` — L3 and R3 — in the standard Linux/AOSP
     * mapping, which is what [Gamepad.genericKeyCode] says they are. They mean touchpad click and
     * mute ONLY inside the straight-through enumeration a driverless Sony pad uses. Read as
     * touchpad and mute anywhere else, every other pad would lose both stick clicks.
     */
    @Test
    fun `every other pad keeps L3 and R3 on those scancodes`() {
        for (p in listOf(
            Gamepad.PadButtons.NATIVE,
            Gamepad.PadButtons.GENERIC_XBOX,
            Gamepad.PadButtons.SONY_MODERN,
        )) {
            val l3 = p.correct(0x13d, Gamepad.genericKeyCode(0x13d))
            val r3 = p.correct(0x13e, Gamepad.genericKeyCode(0x13e))
            assertEquals("$p L3", KeyEvent.KEYCODE_BUTTON_THUMBL, l3)
            assertEquals("$p R3", KeyEvent.KEYCODE_BUTTON_THUMBR, r3)
            assertEquals("$p L3 bit", Gamepad.BTN_LS_CLICK, Gamepad.buttonBit(l3))
            assertEquals("$p R3 bit", Gamepad.BTN_RS_CLICK, Gamepad.buttonBit(r3))
        }
    }

    /** An Xbox-layout pad numbering straight through: A B X Y LB RB View Menu LS RS. */
    @Test
    fun `an Xbox pad numbering straight through keeps its own layout`() {
        assertEquals(KeyEvent.KEYCODE_BUTTON_A, xbox(0x130))
        assertEquals(KeyEvent.KEYCODE_BUTTON_B, xbox(0x131))
        assertEquals(KeyEvent.KEYCODE_BUTTON_X, xbox(0x132))
        assertEquals(KeyEvent.KEYCODE_BUTTON_Y, xbox(0x133))
        assertEquals(KeyEvent.KEYCODE_BUTTON_L1, xbox(0x134))
        assertEquals(KeyEvent.KEYCODE_BUTTON_R1, xbox(0x135))
        assertEquals(KeyEvent.KEYCODE_BUTTON_SELECT, xbox(0x136)) // View
        assertEquals(KeyEvent.KEYCODE_BUTTON_START, xbox(0x137)) // Menu
        assertEquals(KeyEvent.KEYCODE_BUTTON_THUMBL, xbox(0x138))
        assertEquals(KeyEvent.KEYCODE_BUTTON_THUMBR, xbox(0x139))
    }

    /** `hid-playstation` emits the modern Linux codes, where only the face pair reads swapped. */
    @Test
    fun `a driver-backed Sony pad has only its face pair corrected`() {
        val m = Gamepad.PadButtons.SONY_MODERN
        assertEquals(KeyEvent.KEYCODE_BUTTON_Y, m.correct(0x133, KeyEvent.KEYCODE_BUTTON_X)) // △
        assertEquals(KeyEvent.KEYCODE_BUTTON_X, m.correct(0x134, KeyEvent.KEYCODE_BUTTON_Y)) // □
        for (scan in listOf(0x130, 0x131, 0x136, 0x137, 0x13a, 0x13b, 0x13c)) {
            assertEquals(Gamepad.genericKeyCode(scan), m.correct(scan, Gamepad.genericKeyCode(scan)))
        }
    }

    /**
     * The guard that makes all of this safe to run on every pad: a keycode that is NOT what
     * `Generic.kl` would have said came from a device-specific key layout, which knows this
     * controller better than any table here. Correcting it would break a pad that works.
     */
    @Test
    fun `a keycode a device layout already resolved is never second-guessed`() {
        // AOSP's DualSense layout puts △ on BUTTON_Y itself. Every profile must leave it be.
        for (p in Gamepad.PadButtons.entries) {
            assertEquals(KeyEvent.KEYCODE_BUTTON_Y, p.correct(0x133, KeyEvent.KEYCODE_BUTTON_Y))
        }
        // Same for a scancode outside the generic gamepad block entirely — a pad's Back key.
        assertEquals(
            KeyEvent.KEYCODE_BACK,
            Gamepad.PadButtons.GENERIC_SONY.correct(158, KeyEvent.KEYCODE_BACK),
        )
    }

    /**
     * The guard's NEGATIVE path. These fail without it: a device-specific key layout answering
     * something the table disagrees with, on a scancode the table has an opinion about. The
     * layout wins — it knows this controller, and the table is only ever a guess about a pad
     * nothing knew.
     */
    @Test
    fun `a device layout outranks the table on a scancode the table would have rewritten`() {
        // `Generic.kl` calls 0x134 BUTTON_Y, and GENERIC_SONY/GENERIC_XBOX both rewrite that
        // scancode to BUTTON_L1. A layout that says BUTTON_X must survive both.
        for (p in listOf(Gamepad.PadButtons.GENERIC_SONY, Gamepad.PadButtons.GENERIC_XBOX)) {
            assertEquals("$p", KeyEvent.KEYCODE_BUTTON_X, p.correct(0x134, KeyEvent.KEYCODE_BUTTON_X))
        }
        // And the two rows added for the touchpad and mute are no different: a pad whose layout
        // resolved 0x13d itself keeps that answer rather than the borrowed BUTTON_15.
        assertEquals(
            KeyEvent.KEYCODE_BUTTON_1,
            Gamepad.PadButtons.GENERIC_SONY.correct(0x13d, KeyEvent.KEYCODE_BUTTON_1),
        )
    }

    /** Correcting twice is correcting once — the output is never itself a generic-layout answer. */
    @Test
    fun `correction is idempotent`() {
        for (p in Gamepad.PadButtons.entries) {
            for (scan in 0x130..0x13e) {
                val once = p.correct(scan, Gamepad.genericKeyCode(scan))
                assertEquals(once, p.correct(scan, once))
            }
        }
    }

    /**
     * The axis half. A pad that names its triggers something Android knows is read by Android's
     * own axis names — the fallback must NOT fire on the pads that already work.
     */
    @Test
    fun `a pad that names its triggers is read unchanged`() {
        for (p in Gamepad.PadButtons.entries) {
            val map = Gamepad.padMap(p, namedTriggers = true, hasRxRy = true, restsNegative = true)
            assertEquals(MotionEvent.AXIS_Z, map.rightStickX)
            assertEquals(MotionEvent.AXIS_RZ, map.rightStickY)
            assertEquals(Gamepad.AXIS_NONE, map.leftTrigger)
            assertEquals(Gamepad.AXIS_NONE, map.rightTrigger)
        }
        // Same when there is no Rx/Ry to fall back to in the first place.
        val none = Gamepad.padMap(Gamepad.PadButtons.GENERIC_SONY, false, hasRxRy = false, restsNegative = false)
        assertEquals(Gamepad.AXIS_NONE, none.leftTrigger)
    }

    /**
     * A pad with no named trigger and no Rx/Ry — a Switch Pro under AOSP's layout, ZL/ZR as
     * BUTTON_L2/R2 — takes its triggers from those keys. A pad with a trigger axis never does,
     * or the key edge and the axis would fight over one wire value.
     */
    @Test
    fun `a pad with no trigger axis takes its triggers from the L2 and R2 keys`() {
        for (p in Gamepad.PadButtons.entries) {
            fun digital(named: Boolean, rxRy: Boolean) =
                Gamepad.padMap(p, namedTriggers = named, hasRxRy = rxRy, restsNegative = false).digitalTriggers
            assertTrue(digital(named = false, rxRy = false))
            assertFalse(digital(named = false, rxRy = true))
            assertFalse(digital(named = true, rxRy = false))
            assertFalse(digital(named = true, rxRy = true))
        }
    }

    /**
     * A Sony pad reporting straight through lays out X, Y, Z, Rz, Rx, Ry — left stick, right
     * stick, then the triggers. Only the triggers are added; the sticks already read right and
     * must be left alone.
     */
    @Test
    fun `an unmapped Sony pad keeps its sticks and gains its triggers`() {
        val map = Gamepad.padMap(Gamepad.PadButtons.GENERIC_SONY, false, hasRxRy = true, restsNegative = false)
        assertEquals(MotionEvent.AXIS_Z, map.rightStickX)
        assertEquals(MotionEvent.AXIS_RZ, map.rightStickY)
        assertEquals(MotionEvent.AXIS_RX, map.leftTrigger)
        assertEquals(MotionEvent.AXIS_RY, map.rightTrigger)
    }

    /**
     * Every other unmapped pad is the opposite way round: right stick on Rx/Ry, triggers on Z/Rz.
     * Reading Z/Rz as the right stick there is what makes pulling a trigger swing it — so the two
     * pairs must never be mixed up, which is the whole point of pinning them.
     */
    @Test
    fun `an unmapped Xbox-layout pad has its stick and triggers the other way round`() {
        for (p in listOf(Gamepad.PadButtons.GENERIC_XBOX, Gamepad.PadButtons.SONY_MODERN)) {
            val map = Gamepad.padMap(p, namedTriggers = false, hasRxRy = true, restsNegative = false)
            assertEquals(MotionEvent.AXIS_RX, map.rightStickX)
            assertEquals(MotionEvent.AXIS_RY, map.rightStickY)
            assertEquals(MotionEvent.AXIS_Z, map.leftTrigger)
            assertEquals(MotionEvent.AXIS_RZ, map.rightTrigger)
        }
    }

    /**
     * A trigger axis that idles at −1 is rescaled; one that idles at 0 must NOT be, or it would
     * read as a permanent half-pull. Which it is gets measured off the device, never assumed. A
     * stock mapper centres Rx/Ry/Z/Rz on −1..1, so the DualSense's raw RX/RY and the Xbox pad's
     * Z/Rz both rest at −1 there.
     */
    @Test
    fun `only a trigger that idles negative is rescaled`() {
        val signed = Gamepad.padMap(Gamepad.PadButtons.GENERIC_SONY, false, hasRxRy = true, restsNegative = true)
        assertEquals(0f, signed.level(-1f), 1e-6f)
        assertEquals(0.5f, signed.level(0f), 1e-6f)
        assertEquals(1f, signed.level(1f), 1e-6f)

        val unsigned = Gamepad.padMap(Gamepad.PadButtons.GENERIC_SONY, false, hasRxRy = true, restsNegative = false)
        assertEquals(0f, unsigned.level(0f), 1e-6f)
        assertEquals(1f, unsigned.level(1f), 1e-6f)
    }

    /** A pad Android does know is untouched, which is most of them. */
    @Test
    fun `a pad with a key layout is left alone`() {
        for (scan in 0x130..0x13e) {
            val generic = Gamepad.genericKeyCode(scan)
            assertEquals(generic, Gamepad.PadButtons.NATIVE.correct(scan, generic))
        }
    }

    /**
     * `hasKeys` says BUTTON_C and BUTTON_Z for any pad that DECLARES six buttons — `hid-input`
     * allocates the whole descriptor `BTN_A + n` straight through whether the pad ever presses
     * them or not — so a declaration is not a report order. Naming the triggers is what tells a
     * standard-layout pad from a straight-through one.
     */
    @Test
    fun `a pad that names its triggers is never corrected, whatever it declares`() {
        for (sony in listOf(false, true)) {
            for (declaresCZ in listOf(false, true)) {
                assertEquals(
                    Gamepad.PadButtons.NATIVE,
                    Gamepad.padButtons(namedTriggers = true, sony = sony, declaresCZ = declaresCZ),
                )
            }
        }
    }

    /**
     * On a pad whose report order is already standard, NATIVE keeps X, Y and both shoulders
     * themselves. The correction tables are right for the pads they are for; this is about not
     * reaching one of them.
     */
    @Test
    fun `an Xbox pad at the standard positions keeps X, Y and its shoulders`() {
        val native = Gamepad.PadButtons.NATIVE
        assertEquals(KeyEvent.KEYCODE_BUTTON_X, native.correct(0x133, KeyEvent.KEYCODE_BUTTON_X))
        assertEquals(KeyEvent.KEYCODE_BUTTON_Y, native.correct(0x134, KeyEvent.KEYCODE_BUTTON_Y))
        assertEquals(KeyEvent.KEYCODE_BUTTON_L1, native.correct(0x136, KeyEvent.KEYCODE_BUTTON_L1))
        assertEquals(KeyEvent.KEYCODE_BUTTON_R1, native.correct(0x137, KeyEvent.KEYCODE_BUTTON_R1))
        // GENERIC_XBOX on the same events: X as Y, Y as LB, the shoulders as menu keys.
        val wrong = Gamepad.PadButtons.GENERIC_XBOX
        assertEquals(KeyEvent.KEYCODE_BUTTON_Y, wrong.correct(0x133, KeyEvent.KEYCODE_BUTTON_X))
        assertEquals(KeyEvent.KEYCODE_BUTTON_L1, wrong.correct(0x134, KeyEvent.KEYCODE_BUTTON_Y))
        assertEquals(KeyEvent.KEYCODE_BUTTON_SELECT, wrong.correct(0x136, KeyEvent.KEYCODE_BUTTON_L1))
        assertEquals(KeyEvent.KEYCODE_BUTTON_START, wrong.correct(0x137, KeyEvent.KEYCODE_BUTTON_R1))
    }

    /**
     * Switchroot's combined Joy-Cons (`057e:2008`): the kernel sends faces by position, the ROM's
     * layout names them by engraved letter. Pairs are (scancode, keycode that layout delivers).
     * AOSP's Pro Controller layout already answers positionally, so there this changes nothing.
     */
    @Test
    fun `a Nintendo pad's faces come from the scancode, not the ROM layout`() {
        val engraved = listOf(
            0x131 to KeyEvent.KEYCODE_BUTTON_A, // engraved A, east
            0x130 to KeyEvent.KEYCODE_BUTTON_B, // engraved B, south
            0x133 to KeyEvent.KEYCODE_BUTTON_X, // engraved X, north
            0x134 to KeyEvent.KEYCODE_BUTTON_Y, // engraved Y, west
        )
        val positional = listOf(
            KeyEvent.KEYCODE_BUTTON_B,
            KeyEvent.KEYCODE_BUTTON_A,
            KeyEvent.KEYCODE_BUTTON_Y,
            KeyEvent.KEYCODE_BUTTON_X,
        )
        for (pid in listOf(0x2008, 0x2009)) {
            for ((i, row) in engraved.withIndex()) {
                assertEquals(positional[i], Gamepad.nintendoFace(0x057E, pid, row.first))
            }
        }
        // D-pad, ZL and Home keep their layout; other pads and unverified Nintendo ids are untouched.
        for (scan in listOf(0x220, 0x138, 0x13c)) assertEquals(null, Gamepad.nintendoFace(0x057E, 0x2008, scan))
        assertEquals(null, Gamepad.nintendoFace(0x045E, 0x2008, 0x131))
        assertEquals(null, Gamepad.nintendoFace(0x057E, 0x2069, 0x131))
    }

    /**
     * An AYN Thor's own pad, measured with `getevent` on the device: in the Nintendo controller
     * style (`2020:0111`) each face scancode follows the print, in the Xbox style (`2020:0112`) the
     * position. Only the Nintendo style on a Thor is corrected.
     */
    @Test
    fun `an AYN Thor's Nintendo style reads its faces by position`() {
        val thor = "AYN Thor"
        assertEquals(KeyEvent.KEYCODE_BUTTON_B, Gamepad.aynThorFace(thor, 0x2020, 0x0111, 0x130)) // printed A, east
        assertEquals(KeyEvent.KEYCODE_BUTTON_A, Gamepad.aynThorFace(thor, 0x2020, 0x0111, 0x131)) // printed B, south
        assertEquals(KeyEvent.KEYCODE_BUTTON_Y, Gamepad.aynThorFace(thor, 0x2020, 0x0111, 0x133)) // printed X, north
        assertEquals(KeyEvent.KEYCODE_BUTTON_X, Gamepad.aynThorFace(thor, 0x2020, 0x0111, 0x134)) // printed Y, west
        // D-pad and shoulders keep their layout; the Xbox style, other AYN models and other pads are untouched.
        for (scan in listOf(0x220, 0x136, 0x13c)) assertEquals(null, Gamepad.aynThorFace(thor, 0x2020, 0x0111, scan))
        assertEquals(null, Gamepad.aynThorFace(thor, 0x2020, 0x0112, 0x130))
        assertEquals(null, Gamepad.aynThorFace("AYN Odin2", 0x2020, 0x0111, 0x130))
        assertEquals(null, Gamepad.aynThorFace(thor, 0x045E, 0x0111, 0x130))
    }

    /** Nintendo pads declare the kind the SDL clients' `pref_for_ids` and `pref_for_type` give. */
    @Test
    fun `a Nintendo pad declares its own kind`() {
        val kinds = mapOf(
            0x2009 to Gamepad.PREF_SWITCHPRO,
            0x2069 to Gamepad.PREF_SWITCH2PRO,
            0x2073 to Gamepad.PREF_SWITCH2GAMECUBE,
            0x2008 to Gamepad.PREF_JOYCONPAIR, // joycond's combined Joy-Cons
            0x2068 to Gamepad.PREF_JOYCONPAIR, // SDL's combined Joy-Con 2
            0x2006 to Gamepad.PREF_XBOX360, // a single half
            0x2067 to Gamepad.PREF_XBOX360,
        )
        for ((pid, kind) in kinds) assertEquals("057e:%04x".format(pid), kind, Gamepad.prefFor(0x057E, pid))
    }

    /** Past the gate, the vendor and the declared C/Z buttons pick the report order. */
    @Test
    fun `an unnamed-trigger pad still resolves its report order`() {
        fun order(sony: Boolean, declaresCZ: Boolean) =
            Gamepad.padButtons(namedTriggers = false, sony = sony, declaresCZ = declaresCZ)
        assertEquals(Gamepad.PadButtons.GENERIC_SONY, order(sony = true, declaresCZ = true))
        assertEquals(Gamepad.PadButtons.GENERIC_XBOX, order(sony = false, declaresCZ = true))
        assertEquals(Gamepad.PadButtons.SONY_MODERN, order(sony = true, declaresCZ = false))
        assertEquals(Gamepad.PadButtons.NATIVE, order(sony = false, declaresCZ = false))
    }
}
