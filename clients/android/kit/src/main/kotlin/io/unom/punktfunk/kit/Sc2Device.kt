package io.unom.punktfunk.kit

/**
 * Steam Controller 2 (2026, Valve "Ibex" / SDL "Triton") protocol constants + the light state
 * parser the CLIENT needs. The full report rides the wire verbatim (`nativeSendPadHidReport` →
 * the host's as-is virtual pad); this parser only extracts what the client itself consumes: the
 * button word for the typed mirror + exit chord, and sticks/triggers for the degrade path.
 *
 * Protocol ground truth: SDL's `SDL_hidapi_steam_triton.c` + `steam/controller_structs.h`
 * (Valve-maintained), mirrored host-side in `punktfunk-host`'s `triton_proto.rs`.
 */
object Sc2Device {
    const val VID_VALVE = 0x28DE

    /** Wired controller. */
    const val PID_WIRED = 0x1302

    /** Direct BLE identity (transport handled by [Sc2BleLink], not USB). */
    const val PID_BLE = 0x1303

    /** The wireless Puck dongles (Proteus / Nereid) — controller on USB interfaces 2..5. */
    const val PID_DONGLE_PROTEUS = 0x1304
    const val PID_DONGLE_NEREID = 0x1305

    val USB_PIDS = setOf(PID_WIRED, PID_DONGLE_PROTEUS, PID_DONGLE_NEREID)

    /** Dongle interface range that carries controllers (SDL: "interfaces 2..5, currently"). */
    val DONGLE_IFACES = 2..5

    // ---- GATT topology + framing (Valve vendor service; cf. Apple's `Sc2Device.swift`) ----

    /** The custom Valve vendor service every SC2 exposes over BLE. */
    const val BLE_SERVICE = "100f6c32-1735-4313-b402-38567131e5f3"

    /**
     * Feature characteristic: where lizard-off and Steam's gyro-enable land. It sits OUTSIDE the
     * per-report output block, so a link that resolves writes by scanning that block alone never
     * finds it and every feature write drives a rumble actuator instead.
     */
    const val BLE_FEATURE_CHAR = "100f6c34-1735-4313-b402-38567131e5f3"

    /**
     * Per-OUTPUT-report characteristic: Valve routes report id `0xNN` to `100F6C<NN+0x35>`, and
     * the id only SELECTS the characteristic — it is stripped from the payload written there.
     * Verified per-actuator on hardware: the 0x82 test buzz is silent when mis-routed to 0x80's.
     */
    fun bleOutputChar(id: Int): String =
        "100f6c%02x-1735-4313-b402-38567131e5f3".format((id + 0x35) and 0xFF)

    /**
     * Declared STRIPPED payload length per output report id (wire length = stripped + 1), or null
     * for an id with no declared length: clamp to what arrived, never guess-trim past it.
     *
     * The host's `pf_driver_proto::triton::out_report_len` holds the same table id-INCLUDED;
     * `clients/shared/sc2-vectors.json` holds it, the Apple table and this one to the same rows.
     */
    fun strippedOutputLen(id: Int): Int? = when (id) {
        0x80 -> 9 // grip rumble    -> 100F6CB5
        0x81 -> 7 // trackpad pulse -> 100F6CB6
        0x82 -> 3 // haptic command -> 100F6CB7 (Steam's ping/test buzz)
        0x83 -> 9 // LFO tone       -> 100F6CB8
        0x84 -> 8 // log sweep      -> 100F6CB9
        0x85 -> 3 // script         -> 100F6CBA
        0x86 -> 3 // vendor         -> 100F6CBB
        0x87, 0x88, 0x89 -> 63 // vendor big -> 100F6CBC/BD/BE
        else -> null
    }

    /** Grip-rumble output report — the id Steam drives both motors with. */
    const val ID_OUT_RUMBLE = 0x80

    /**
     * The pending-OUT queue key for an output frame. Steam re-sends [ID_OUT_RUMBLE] grip rumble as
     * a level, so a newer one supersedes the pending one; every other id is a one-shot that must
     * not be lost.
     */
    fun outputCoalesceKey(frame: ByteArray): Int =
        if (frame.firstOrNull() == ID_OUT_RUMBLE.toByte()) {
            OutReportQueue.KEY_RUMBLE
        } else {
            OutReportQueue.NO_COALESCE
        }

    /**
     * `MsgHapticRumble`: `[0x80][type][intensity u16][left.speed u16][left.gain i8]
     * [right.speed u16][right.gain i8]`, little-endian, 10 bytes on the wire — the same offsets
     * the host reads back in `triton_proto::parse_triton_rumble`. Type 0 and gain 0 are the
     * unattenuated default; a frame of zero speeds is what stops the motors again.
     */
    fun rumbleFrame(left: Int, right: Int): ByteArray = ByteArray(10).also {
        it[0] = ID_OUT_RUMBLE.toByte()
        it[2] = 0xFF.toByte() // intensity: full scale
        it[3] = 0xFF.toByte()
        it[4] = (left and 0xFF).toByte()
        it[5] = ((left shr 8) and 0xFF).toByte()
        it[7] = (right and 0xFF).toByte()
        it[8] = ((right shr 8) and 0xFF).toByte()
    }

    /**
     * Incoming: a GATT characteristic value is the bare payload with no HID report-id byte, so
     * re-prepend [ID_STATE_BLE] on state-sized (40 B and up) payloads — the wire then carries the
     * same id-first framing as USB. Short payloads (battery/status) pass through unmodified.
     */
    fun frameIncoming(payload: ByteArray): ByteArray =
        if (payload.size < 40) {
            payload
        } else {
            ByteArray(payload.size + 1).also {
                it[0] = ID_STATE_BLE.toByte()
                payload.copyInto(it, 1)
            }
        }

    /** One resolved OUTPUT write: the per-report characteristic, and the bare payload for it. */
    class OutputWrite(val charUuid: String, val payload: ByteArray)

    /**
     * Outgoing OUTPUT (`kind` 0): the frame arrives id-first, the id selects the characteristic
     * and is stripped, and the payload is trimmed to [strippedOutputLen] clamped to what arrived.
     * The clamp earns its keep — an older host pads every frame to 64 B, and the write must carry
     * exactly the declared length either way. Null for a frame too short to carry a payload.
     */
    fun outputWrite(frame: ByteArray): OutputWrite? {
        if (frame.size < 2) return null
        val id = frame[0].toInt() and 0xFF
        val n = (strippedOutputLen(id) ?: (frame.size - 1)).coerceAtMost(frame.size - 1)
        return OutputWrite(bleOutputChar(id), frame.copyOfRange(1, 1 + n))
    }

    /**
     * Outgoing FEATURE (`kind` 1): strip the leading `0x01` channel report-id and write the rest
     * to [BLE_FEATURE_CHAR] whole, zero padding included. The characteristic value carries no
     * channel byte, so the firmware reads byte 0 as the command id: unstripped it parses as
     * command `0x01` and lizard-off fails silently. Null for a frame with no command.
     */
    fun featurePayload(frame: ByteArray): ByteArray? =
        if (frame.size < 2) null else frame.copyOfRange(1, frame.size)

    // Input report ids (`ETritonReportIDTypes`). State layouts share every offset the client
    // reads (seq/buttons/triggers/sticks); 0x47 only diverges from byte 18 (trackpad timestamp).
    const val ID_STATE = 0x42
    const val ID_BATTERY = 0x43
    const val ID_STATE_BLE = 0x45
    const val ID_WIRELESS_X = 0x46
    const val ID_STATE_TIMESTAMP = 0x47
    const val ID_WIRELESS = 0x79

    /** Wireless status payload byte: controller connected/disconnected through the Puck. */
    const val WIRELESS_DISCONNECT = 1
    const val WIRELESS_CONNECT = 2

    // Button bits in the state report's u32 (SDL `TritonButtons`). SDL's enum swaps the View and
    // Menu names; its mapping and hid-steam agree with these values.
    const val A = 0x00000001
    const val B = 0x00000002
    const val X = 0x00000004
    const val Y = 0x00000008
    const val QAM = 0x00000010
    const val R3 = 0x00000020
    const val MENU = 0x00000040
    const val R4 = 0x00000080
    const val R5 = 0x00000100
    const val RB = 0x00000200
    const val DPAD_DOWN = 0x00000400
    const val DPAD_RIGHT = 0x00000800
    const val DPAD_LEFT = 0x00001000
    const val DPAD_UP = 0x00002000
    const val VIEW = 0x00004000
    const val L3 = 0x00008000
    const val STEAM = 0x00010000
    const val L4 = 0x00020000
    const val L5 = 0x00040000
    const val LB = 0x00080000
    const val RPAD_CLICK = 0x00400000

    /**
     * The feature report that turns lizard mode (built-in keyboard/mouse emulation) off:
     * `[report id 1][ID_SET_SETTINGS_VALUES 0x87][length 3][SETTING_LIZARD_MODE 9]
     * [LIZARD_MODE_OFF u16]`, zero-padded to the 64-byte feature size. The firmware watchdog
     * re-enables lizard mode after a few seconds of silence, so this is re-sent every
     * [LIZARD_REFRESH_MS] (SDL's cadence) — and the host's Steam sends its own through the raw
     * plane once it grabs the virtual pad, which lands here too.
     */
    val DISABLE_LIZARD: ByteArray = ByteArray(64).also {
        it[0] = 0x01 // feature report id
        it[1] = 0x87.toByte() // ID_SET_SETTINGS_VALUES
        it[2] = 3 // one ControllerSetting {u8 num, u16 value}
        it[3] = 9 // SETTING_LIZARD_MODE
        // [4..6] = LIZARD_MODE_OFF (0) — already zero
    }

    /**
     * Force firmware-calibrated signed i16 stick coordinates. Steam sends this during physical
     * controller initialization (`SETTING_ENABLE_RAW_JOYSTICK` = 0x2e, value 0); without it a
     * controller previously opened in raw mode reports ADC coordinates around 0..3200, which a
     * Triton consumer interprets as only a few percent of full travel.
     */
    val NORMALIZE_JOYSTICKS: ByteArray = ByteArray(64).also {
        it[0] = 0x01 // feature report id
        it[1] = 0x87.toByte() // ID_SET_SETTINGS_VALUES
        it[2] = 3 // one ControllerSetting {u8 num, u16 value}
        it[3] = 0x2E // SETTING_ENABLE_RAW_JOYSTICK
        // [4..6] = disabled (0) — firmware emits calibrated signed i16 values
    }

    /**
     * Lizard mode back ON — the same settings write, value non-zero. The claim removes the pad
     * from the OS input stack entirely, and lizard's kb/mouse is what navigates Android TV, so a
     * capture restores it as it releases: the firmware watchdog would, but only after seconds of
     * a dead pad.
     */
    val ENABLE_LIZARD: ByteArray = ByteArray(64).also {
        it[0] = 0x01 // feature report id
        it[1] = 0x87.toByte() // ID_SET_SETTINGS_VALUES
        it[2] = 3 // one ControllerSetting {u8 num, u16 value}
        it[3] = 9 // SETTING_LIZARD_MODE
        it[4] = 1 // LIZARD_MODE_ON (u16 little-endian)
    }

    const val LIZARD_REFRESH_MS = 3000L

    /** Wire mapping: SC2 button bit → punktfunk `Gamepad.BTN_*`, the inverse of the host's
     *  typed-fallback mapping (`triton_proto::from_gamepad`): paddles R4/L4/R5/L5 =
     *  PADDLE1/2/3/4, QAM = MISC1, right-pad click = the touchpad wire bit. */
    private val WIRE_MAP = intArrayOf(
        A, Gamepad.BTN_A,
        B, Gamepad.BTN_B,
        X, Gamepad.BTN_X,
        Y, Gamepad.BTN_Y,
        LB, Gamepad.BTN_LB,
        RB, Gamepad.BTN_RB,
        VIEW, Gamepad.BTN_BACK,
        MENU, Gamepad.BTN_START,
        STEAM, Gamepad.BTN_GUIDE,
        L3, Gamepad.BTN_LS_CLICK,
        R3, Gamepad.BTN_RS_CLICK,
        DPAD_UP, Gamepad.BTN_DPAD_UP,
        DPAD_DOWN, Gamepad.BTN_DPAD_DOWN,
        DPAD_LEFT, Gamepad.BTN_DPAD_LEFT,
        DPAD_RIGHT, Gamepad.BTN_DPAD_RIGHT,
        QAM, Gamepad.BTN_MISC1,
        R4, Gamepad.BTN_PADDLE1,
        L4, Gamepad.BTN_PADDLE2,
        R5, Gamepad.BTN_PADDLE3,
        L5, Gamepad.BTN_PADDLE4,
        RPAD_CLICK, Gamepad.BTN_TOUCHPAD,
    )

    /** Translate an SC2 button word into the wire `Gamepad.BTN_*` bitmask. */
    fun wireButtons(sc2: Int): Int {
        var out = 0
        var i = 0
        while (i < WIRE_MAP.size) {
            if (sc2 and WIRE_MAP[i] != 0) out = out or WIRE_MAP[i + 1]
            i += 2
        }
        return out
    }

    /** The typed-mirror fields of one state report (buttons/sticks/triggers only). */
    class State {
        var buttons = 0 // SC2 bit layout
        var lsX = 0; var lsY = 0 // i16, +y = up (device convention = wire convention)
        var rsX = 0; var rsY = 0
        var lt = 0; var rt = 0 // 0..255 (device 0..32767 scaled down)
    }

    /**
     * Parse the client-consumed fields out of a state report (`0x42`/`0x45`/`0x47` — identical
     * offsets for everything read here) into [out]. Returns false for non-state / short reports.
     */
    fun parseState(report: ByteArray, len: Int, out: State): Boolean {
        if (len < 18) return false
        when (report[0].toInt() and 0xFF) {
            ID_STATE, ID_STATE_BLE, ID_STATE_TIMESTAMP -> {}
            else -> return false
        }
        fun i16(o: Int) = ((report[o + 1].toInt() shl 8) or (report[o].toInt() and 0xFF)).toShort().toInt()
        out.buttons = (report[2].toInt() and 0xFF) or
            ((report[3].toInt() and 0xFF) shl 8) or
            ((report[4].toInt() and 0xFF) shl 16) or
            ((report[5].toInt() and 0xFF) shl 24)
        out.lt = (i16(6).coerceIn(0, 32767)) shr 7
        out.rt = (i16(8).coerceIn(0, 32767)) shr 7
        out.lsX = i16(10); out.lsY = i16(12)
        out.rsX = i16(14); out.rsY = i16(16)
        return true
    }
}
