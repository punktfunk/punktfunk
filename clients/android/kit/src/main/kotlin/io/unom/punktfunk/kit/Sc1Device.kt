package io.unom.punktfunk.kit

/**
 * The 2015 Steam Controller (SDL "D0G") as [Sc2Capture] captures it over USB. Its reports and
 * feature frames carry no report id: a version word of 1, then the frame type. The raw report
 * goes to the host as-is; this parser only feeds the typed mirror and menu navigation, in the
 * Steam Controller 2's button layout so that code serves both pads unchanged.
 *
 * Ground truth: SDL `SDL_hidapi_steam.c` and `steam/controller_{constants,structs}.h`; the
 * haptic pulse's swapped sides come from the kernel's `hid-steam.c`. The tables shared with the
 * Apple client and core are pinned by `clients/shared/sc1-vectors.json`.
 */
object Sc1Device {
    /** The wired pad; Steam claims it on [WIRED_IFACE]. */
    const val PID_WIRED = 0x1102

    /** The wireless dongle: one pad per interface in [DONGLE_IFACES]. */
    const val PID_DONGLE = 0x1142

    val USB_PIDS = setOf(PID_WIRED, PID_DONGLE)

    const val WIRED_IFACE = 2
    val DONGLE_IFACES = 1..4

    /** Frame types (`ValveInReportHeader_t.ucType`). */
    const val TYPE_STATE = 0x01
    const val TYPE_WIRELESS = 0x03

    /** Wireless event byte: the pad behind a dongle slot powered off. */
    const val WIRELESS_DISCONNECT = 1

    // State-report button bits (the 24-bit word at bytes 8..11).
    private const val LPAD_CLICK = 0x020000
    private const val LPAD_TOUCH = 0x080000
    private const val RPAD_TOUCH = 0x100000

    /** Left pad and stick share 16..20; with this set the pad holds them and the stick last reported. */
    private const val LPAD_AND_STICK = 0x800000

    /** 2015 bit → Steam Controller 2 bit. Grips land on L4/R4, the right-pad click on its own. */
    private val TO_SC2 = intArrayOf(
        0x000004, Sc2Device.RB,
        0x000008, Sc2Device.LB,
        0x000010, Sc2Device.Y,
        0x000020, Sc2Device.B,
        0x000040, Sc2Device.X,
        0x000080, Sc2Device.A,
        0x000100, Sc2Device.DPAD_UP,
        0x000200, Sc2Device.DPAD_RIGHT,
        0x000400, Sc2Device.DPAD_LEFT,
        0x000800, Sc2Device.DPAD_DOWN,
        0x001000, Sc2Device.VIEW,
        0x002000, Sc2Device.STEAM,
        0x004000, Sc2Device.MENU,
        0x008000, Sc2Device.L4,
        0x010000, Sc2Device.R4,
        0x040000, Sc2Device.RPAD_CLICK,
        0x400000, Sc2Device.L3,
    )

    /** The 2015 button word in the Steam Controller 2's layout. */
    fun sc2Buttons(word: Int): Int {
        var out = 0
        var i = 0
        while (i < TO_SC2.size) {
            if (word and TO_SC2[i] != 0) out = out or TO_SC2[i + 1]
            i += 2
        }
        // Older firmware reports a stick click as a left-pad click with no finger on the pad.
        if (word and LPAD_CLICK != 0 && word and LPAD_TOUCH == 0) out = out or Sc2Device.L3
        return out
    }

    /**
     * The typed-mirror fields of a state report into [out], buttons in the Steam Controller 2's
     * layout. False for any other frame. A touched left pad leaves the stick where it last was
     * when the pad interleaves both, else centred; the right pad is the right stick while touched.
     */
    fun parseState(report: ByteArray, len: Int, out: Sc2Device.State): Boolean {
        if (len < 24 || !isFrame(report, len, TYPE_STATE)) return false
        fun u8(o: Int) = report[o].toInt() and 0xFF
        fun i16(o: Int) = ((report[o + 1].toInt() shl 8) or u8(o)).toShort().toInt()
        val word = u8(8) or (u8(9) shl 8) or (u8(10) shl 16)
        out.buttons = sc2Buttons(word)
        if (word and LPAD_TOUCH == 0) {
            out.lsX = i16(16); out.lsY = i16(18)
        } else if (word and LPAD_AND_STICK == 0) {
            out.lsX = 0; out.lsY = 0
        }
        if (word and RPAD_TOUCH != 0) {
            out.rsX = i16(20); out.rsY = i16(22)
        } else {
            out.rsX = 0; out.rsY = 0
        }
        out.lt = trigger(u8(11))
        out.rt = trigger(u8(12))
        return true
    }

    /** SDL's scale: `(v << 7) | v` reaches full at 26000, so a pull of 202 is full. */
    private fun trigger(v: Int): Int = (v * 129 * 255 / 26000).coerceAtMost(255)

    /** Whether [report] is a frame of [type]: version word 1, then the type. */
    fun isFrame(report: ByteArray, len: Int, type: Int): Boolean =
        len >= 3 && report[0].toInt() == 1 && report[1].toInt() == 0 &&
            report[2].toInt() and 0xFF == type

    /** Whether [report] says the pad behind this dongle slot powered off. */
    fun isWirelessDisconnect(report: ByteArray, len: Int): Boolean =
        len >= 5 && isFrame(report, len, TYPE_WIRELESS) &&
            report[4].toInt() and 0xFF == WIRELESS_DISCONNECT

    /** A feature frame as the pad takes it: report id 0, then [body], zero-padded to 65 bytes. */
    private fun frame(vararg body: Int): ByteArray = ByteArray(65).also { f ->
        body.forEachIndexed { i, b -> f[i + 1] = b.toByte() }
    }

    /**
     * Lizard mode off, as SDL's driver sends it at open: digital mappings cleared, settings reset,
     * then packet version 2 and both trackpads emulating nothing. The pad has no watchdog, so
     * once per claim.
     */
    val LIZARD_OFF: List<ByteArray> = listOf(
        frame(0x81),
        frame(0x8E, 0),
        frame(0x87, 12, 0x31, 2, 0, 7, 7, 0, 8, 7, 0, 24, 0, 0),
    )

    /** Lizard mode back on as the claim goes back, as SDL's driver closes: the OS gets its mouse. */
    val LIZARD_ON: List<ByteArray> = listOf(
        frame(0x85),
        frame(0x8E, 0),
        frame(0x87, 3, 8, 0, 0),
    )

    /**
     * One trackpad haptic pulse train (`0x8F`): [pad] 0 right, 1 left; [onUs] high and [offUs]
     * low per period, [count] periods.
     */
    fun hapticPulse(pad: Int, onUs: Int, offUs: Int, count: Int): ByteArray = frame(
        0x8F, 7, pad,
        onUs and 0xFF, (onUs shr 8) and 0xFF,
        offUs and 0xFF, (offUs shr 8) and 0xFF,
        count and 0xFF, (count shr 8) and 0xFF,
    )
}
