package io.unom.punktfunk.kit

import kotlin.math.abs

/**
 * Sony DualSense / DualSense Edge / DualShock 4 **USB** protocol constants: the input-report
 * parser and the output-report builders the capture link ([DsCapture]) needs. Unlike the SC2's
 * as-is passthrough, nothing rides the wire raw here — the host's DualSense/DS4 backends consume
 * only typed events (`dualsense_proto.rs` discards `RichInput::HidReport`), so the client parses
 * the pad's input reports into the ordinary button/axis wire + the rich touch/motion plane, and
 * renders the host's feedback (rumble / adaptive triggers / lightbar / player LEDs) by composing
 * USB output reports itself.
 *
 * Protocol ground truth: the Linux kernel's `hid-playstation` / `hid-sony` structs, SDL's
 * `SDL_hidapi_ps5.c` / `SDL_hidapi_ps4.c`, mirrored host-side in `punktfunk-host`'s
 * `dualsense_proto.rs` / `dualshock4_proto.rs` — this file is the byte-exact inverse of those
 * serializers (offsets cross-referenced below). USB only: over Bluetooth the reports shift
 * (`0x31` + CRC32) AND Android exposes no raw path to a Classic pad anyway, so the BT case never
 * reaches this code — an uncaptured pad stays on the ordinary InputDevice path.
 */
object DsDevice {
    const val VID_SONY = 0x054C
    const val PID_DUALSENSE = 0x0CE6
    const val PID_DUALSENSE_EDGE = 0x0DF2
    const val PID_DUALSHOCK4_V1 = 0x05C4
    const val PID_DUALSHOCK4_V2 = 0x09CC

    val USB_PIDS = setOf(PID_DUALSENSE, PID_DUALSENSE_EDGE, PID_DUALSHOCK4_V1, PID_DUALSHOCK4_V2)

    /**
     * One captured model: its `GamepadPref` wire byte (the virtual pad the host builds — matching
     * the physical one), its output-report size (the descriptor-declared size the firmware
     * expects: DS5 48 = id + 47, Edge 64 = id + 63, DS4 32 = id + 31), its touchpad extent
     * (`dualsense_proto::DS_TOUCH_W/H`, `dualshock4_proto::DS4_TOUCH_*`) for normalizing touches
     * onto the wire's 0..65535 space, and the IMU-calibration feature report it answers
     * ([MotionCal]): DS5/Edge `0x05` (id + 40 B), DS4 over USB `0x02` (id + 36 B).
     */
    enum class Model(
        val pref: Int,
        val outputSize: Int,
        val touchW: Int,
        val touchH: Int,
        val calReportId: Int,
        val calReportLen: Int,
    ) {
        DUALSENSE(Gamepad.PREF_DUALSENSE, 48, 1920, 1080, 0x05, 41),
        DUALSENSE_EDGE(Gamepad.PREF_DUALSENSEEDGE, 64, 1920, 1080, 0x05, 41),
        DUALSHOCK4(Gamepad.PREF_DUALSHOCK4, 32, 1920, 942, 0x02, 37),
    }

    /**
     * One pad's own IMU calibration: the factory scale factors that turn its raw motion counts
     * into the wire's fixed units (`punktfunk_core::input::gamepad` — 20 LSB per °/s, 10000 LSB
     * per g), read out of the calibration feature report the pad serves on EP0.
     *
     * **Why the pad's blob and not a constant.** Measured on glass 2026-08-07: a DualSense flat
     * and face up arrived as 0.811 g where 1.000 was owed, because this path forwarded the raw
     * i16s verbatim. The nominal ×10000/8192 rescale that first closed that gap ([NOMINAL]) still
     * leaves that unit's factory bias — about 1 % — on acceleration, and provably cannot fix gyro
     * at all: the same still-average showed this pad's gyro calibration is nowhere near identity,
     * and a near-identity one would mean 1024 LSB per °/s, i.e. ±32 °/s full scale, which no
     * controller has. The scale is per unit; only the pad knows it.
     *
     * The arithmetic is `hid-playstation`'s, and the host's contract test
     * (`crates/pf-inject/tests/motion_contract.rs`, `SonyImuCalibration`) is the same math read
     * from the other end — it applies it to the blobs our *virtual* pads declare and asserts they
     * land on the wire constants. Per axis: gyro `raw × speed_2x × 20 / (|plus − bias| +
     * |minus − bias|)`, accel `(raw − (plus − range/2)) × 20000 / range`, where `range = plus −
     * minus` spans 2 g.
     */
    class MotionCal private constructor(
        /** Per axis: `speed_2x × 20`, over `|plus − bias| + |minus − bias|`. */
        private val gyroNumer: LongArray,
        private val gyroDenom: LongArray,
        /** Per axis: the raw count the pad reads at 0 g, and the raw span of 2 g. */
        private val accelBias: LongArray,
        private val accelRange: LongArray,
    ) {
        /** Raw gyro count on [axis] (0 = pitch, 1 = yaw, 2 = roll) → the wire's 20 LSB per °/s. */
        fun gyroToWire(axis: Int, raw: Int): Int =
            clampWire(raw.toLong() * gyroNumer[axis] / gyroDenom[axis])

        /** Raw acceleration count on [axis] → the wire's 10000 LSB per g, zero point removed. */
        fun accelToWire(axis: Int, raw: Int): Int =
            clampWire((raw - accelBias[axis]) * ACCEL_NUMER / accelRange[axis])

        /**
         * The derived resolutions, for the capture's one-line claim log — the number that says
         * whether a pad's blob was actually read (a real DualSense declares ≈16 LSB/°·s and ≈8192
         * LSB/g; the [NOMINAL] fallback reads back as exactly 20 and 8192).
         */
        override fun toString(): String = buildString {
            append("gyro ")
            for (i in 0 until 3) {
                if (i > 0) append('/')
                append(gyroDenom[i] * WIRE_GYRO_LSB_PER_DEG_S / gyroNumer[i])
            }
            append(" LSB/°·s, accel ")
            for (i in 0 until 3) {
                if (i > 0) append('/')
                append(accelRange[i] / 2)
            }
            append(" LSB/g at ")
            append(accelBias.joinToString("/"))
        }

        /**
         * Both conversions are a >1 multiplier on every pad measured so far, so a real ±4 g slam
         * or a fast flick near full scale would otherwise wrap the i16 and read as an impossible
         * motion in the opposite direction.
         */
        private fun clampWire(v: Long): Int = v.coerceIn(-32768L, 32767L).toInt()

        companion object {
            /** The pads' nominal acceleration resolution — `hid-playstation`'s `DS_ACC_RES_PER_G`. */
            private const val RAW_ACCEL_LSB_PER_G = 8192L
            /**
             * The wire's gyro scale, taken from [Gamepad] rather than restated. These were literal
             * `20L` / `10000L` until the sensor path hoisted the same numbers into one place; a
             * second copy of a unit constant is precisely the defect this whole program opened
             * with, and two of them in one module would be worse than the original.
             *
             * `val`, not `const val`, only because the widening to Long is not a compile-time
             * constant expression. Long here on purpose: the arithmetic below multiplies raw counts
             * by the calibration's speed term before dividing, which overflows an Int.
             */
            private val WIRE_GYRO_LSB_PER_DEG_S = Gamepad.MOTION_GYRO_LSB_PER_DEG_S.toLong()
            /** `MOTION_ACCEL_LSB_PER_G`, doubled — the declared accel range spans 2 g, not 1. */
            private val ACCEL_NUMER = 2L * Gamepad.MOTION_ACCEL_LSB_PER_G
            /** Bytes the layout below reads; the reports themselves are longer (41 / 37). */
            private const val MIN_LEN = 35

            /**
             * What an unreadable pad gets: gyro straight through and accel on the nominal 8192
             * LSB/g. Wrong by that unit's factory bias, and for gyro wrong by however far its
             * scale sits from the wire's 20 — but a pad whose calibration cannot be read is far
             * better off slightly mis-scaled than silent, so this never zeroes motion.
             */
            val NOMINAL = MotionCal(
                LongArray(3) { 1 },
                LongArray(3) { 1 },
                LongArray(3),
                LongArray(3) { 2 * RAW_ACCEL_LSB_PER_G },
            )

            /**
             * Parse a calibration feature report ([Model.calReportId]) — all little-endian i16:
             * `[0]` report id, `[1..7)` gyro bias (pitch, yaw, roll), `[7..19)` gyro plus/minus
             * INTERLEAVED (pitch+, pitch−, yaw+, yaw−, roll+, roll−), `[19..23)` the two speed
             * words, `[23..35)` accel plus/minus (x+, x−, y+, y−, z+, z−).
             *
             * ⚠ Interleaved is the **USB** order. A Bluetooth DualShock 4 groups the three plusses
             * before the three minuses and consumers switch layout on the transport — this path is
             * USB-only by construction (see the file header), so do not "generalise" it.
             *
             * Falls back to [NOMINAL] for a failed read (null), a truncated or foreign reply, and
             * per axis for a degenerate declaration — a clone or broken pad that declares zeroes
             * would otherwise divide by zero (`hid-playstation` guards the same case, for the same
             * reason).
             */
            fun parse(blob: ByteArray?, reportId: Int): MotionCal {
                if (blob == null || blob.size < MIN_LEN) return NOMINAL
                if ((blob[0].toInt() and 0xFF) != reportId) return NOMINAL
                val w = { o: Int ->
                    ((blob[o + 1].toInt() shl 8) or (blob[o].toInt() and 0xFF)).toShort().toLong()
                }
                val speed2x = w(19) + w(21)
                val gyroNumer = LongArray(3)
                val gyroDenom = LongArray(3)
                val accelBias = LongArray(3)
                val accelRange = LongArray(3)
                for (i in 0 until 3) {
                    val bias = w(1 + 2 * i)
                    val denom = abs(w(7 + 4 * i) - bias) + abs(w(9 + 4 * i) - bias)
                    if (speed2x > 0 && denom > 0) {
                        gyroNumer[i] = speed2x * WIRE_GYRO_LSB_PER_DEG_S
                        gyroDenom[i] = denom
                    } else {
                        gyroNumer[i] = 1 // passthrough, as before any calibration existed
                        gyroDenom[i] = 1
                    }
                    val plus = w(23 + 4 * i)
                    val range = plus - w(25 + 4 * i)
                    if (range > 0) {
                        accelBias[i] = plus - range / 2
                        accelRange[i] = range
                    } else {
                        accelBias[i] = 0 // nominal, as NOMINAL above
                        accelRange[i] = 2 * RAW_ACCEL_LSB_PER_G
                    }
                }
                return MotionCal(gyroNumer, gyroDenom, accelBias, accelRange)
            }
        }
    }

    /** The captured [Model] for a USB PID, or null for anything we don't capture. */
    fun modelFor(pid: Int): Model? = when (pid) {
        PID_DUALSENSE -> Model.DUALSENSE
        PID_DUALSENSE_EDGE -> Model.DUALSENSE_EDGE
        PID_DUALSHOCK4_V1, PID_DUALSHOCK4_V2 -> Model.DUALSHOCK4
        else -> null
    }

    /**
     * The client-consumed fields of one input report. `buttons` is already the WIRE bitmask
     * (`Gamepad.BTN_*`) — the parse maps device bits straight to the wire, the exact inverse of
     * the host's `DsState::from_gamepad` (BTN_A ↔ cross, BTN_B ↔ circle, BTN_X ↔ square,
     * BTN_Y ↔ triangle; positional, not glyph-order). Gyro/accel arrive in WIRE units — the wire's
     * `Motion` is a unit passthrough into the virtual pad's report, so the pad's raw counts are
     * rescaled during the parse by the [MotionCal] handed to [parseState]. Touch coordinates stay
     * device-raw here; [DsCapture] normalizes against the model's extent when forwarding.
     */
    class State {
        var buttons = 0
        var lsX = 0; var lsY = 0 // wire i16, +y = up (device is +y down — inverted in the parse)
        var rsX = 0; var rsY = 0
        var lt = 0; var rt = 0 // 0..255
        val gyro = IntArray(3) // wire i16: 20 LSB per °/s (pitch/yaw/roll)
        val accel = IntArray(3) // wire i16: 10000 LSB per g
        val touchActive = BooleanArray(2)
        val touchX = IntArray(2) // raw device coords (0..touchW-1 / 0..touchH-1)
        val touchY = IntArray(2)
    }

    private const val INPUT_ID = 0x01 // USB input report id, DS5 and DS4 alike

    /**
     * Where one model's USB input report 0x01 (64 B) keeps each field — the offsets mirror the
     * host serializers (`dualsense_proto.rs` / `dualshock4_proto.rs` `serialize_state`). The
     * button BITS are the same on both pads; only the bytes they sit in move.
     */
    private class Layout(
        val minLen: Int,
        /** Two stick bytes each, then the two trigger bytes. */
        val sticks: Int,
        val lt: Int,
        val rt: Int,
        /** hat nibble | face buttons; the next two bytes are `btn1` (shoulders, menu, sticks)
         *  and `btn2` (PS, touchpad, and on the DS5 mute + the Edge paddles). */
        val face: Int,
        val motionLen: Int,
        val gyro: Int,
        val accel: Int,
        val touchLen: Int,
        val touch: Int,
    )

    private val DS5_LAYOUT = Layout(
        minLen = 11, sticks = 1, lt = 5, rt = 6, face = 8,
        motionLen = 28, gyro = 16, accel = 22, touchLen = 41, touch = 33,
    )
    private val DS4_LAYOUT = Layout(
        minLen = 10, sticks = 1, lt = 8, rt = 9, face = 5,
        motionLen = 25, gyro = 13, accel = 19, touchLen = 43, touch = 35,
    )

    // face byte high nibble (`btn0`).
    private const val BTN_SQUARE = 0x10
    private const val BTN_CROSS = 0x20
    private const val BTN_CIRCLE = 0x40
    private const val BTN_TRIANGLE = 0x80
    // `btn1`: L1, R1, Create/Share, Options, L3, R3.
    private const val BTN_L1 = 0x01
    private const val BTN_R1 = 0x02
    private const val BTN_CREATE = 0x10
    private const val BTN_OPTIONS = 0x20
    private const val BTN_L3 = 0x40
    private const val BTN_R3 = 0x80
    // `btn2`; mute is DS5-only, the FN/BACK bits exist only on the Edge.
    private const val BTN_PS = 0x01
    private const val BTN_TOUCHPAD = 0x02
    private const val DS5_MUTE = 0x04
    private const val EDGE_FN_LEFT = 0x10
    private const val EDGE_FN_RIGHT = 0x20
    private const val EDGE_BACK_LEFT = 0x40
    private const val EDGE_BACK_RIGHT = 0x80

    /**
     * Parse one USB input report (`0x01`) into [out]. Returns false for any other report id or a
     * short read (the pad also emits `0x09`-family getMAC responses etc. on EP0 — those never hit
     * the interrupt endpoint, but be defensive). Motion/touch fields update only when the report
     * is long enough to carry them (it always is on glass — 64-byte interrupt transfers).
     *
     * [cal] is this pad's own motion calibration, read once when the capture claims it; the
     * default is the nominal fallback, which is all a caller without a live pad (the tests) can
     * have.
     */
    fun parseState(
        model: Model,
        report: ByteArray,
        len: Int,
        out: State,
        cal: MotionCal = MotionCal.NOMINAL,
    ): Boolean {
        val r = report
        val l = if (model == Model.DUALSHOCK4) DS4_LAYOUT else DS5_LAYOUT
        if (len < l.minLen || (r[0].toInt() and 0xFF) != INPUT_ID) return false
        out.lsX = stickX(u8(r, l.sticks))
        out.lsY = stickY(u8(r, l.sticks + 1))
        out.rsX = stickX(u8(r, l.sticks + 2))
        out.rsY = stickY(u8(r, l.sticks + 3))
        out.lt = u8(r, l.lt)
        out.rt = u8(r, l.rt)
        val b0 = u8(r, l.face)
        val b1 = u8(r, l.face + 1)
        val b2 = u8(r, l.face + 2)
        var w = hatBits(b0 and 0x0F)
        if (b0 and BTN_CROSS != 0) w = w or Gamepad.BTN_A
        if (b0 and BTN_CIRCLE != 0) w = w or Gamepad.BTN_B
        if (b0 and BTN_SQUARE != 0) w = w or Gamepad.BTN_X
        if (b0 and BTN_TRIANGLE != 0) w = w or Gamepad.BTN_Y
        if (b1 and BTN_L1 != 0) w = w or Gamepad.BTN_LB
        if (b1 and BTN_R1 != 0) w = w or Gamepad.BTN_RB
        // L2/R2 digital bits ride the analog axes instead (wire convention).
        if (b1 and BTN_CREATE != 0) w = w or Gamepad.BTN_BACK
        if (b1 and BTN_OPTIONS != 0) w = w or Gamepad.BTN_START
        if (b1 and BTN_L3 != 0) w = w or Gamepad.BTN_LS_CLICK
        if (b1 and BTN_R3 != 0) w = w or Gamepad.BTN_RS_CLICK
        if (b2 and BTN_PS != 0) w = w or Gamepad.BTN_GUIDE
        if (b2 and BTN_TOUCHPAD != 0) w = w or Gamepad.BTN_TOUCHPAD
        if (model != Model.DUALSHOCK4 && b2 and DS5_MUTE != 0) w = w or Gamepad.BTN_MISC1
        if (model == Model.DUALSENSE_EDGE) {
            // Wire paddle order matches the host's `edge_paddle_bits` inverse: PADDLE1/2 =
            // right/left BACK (the primary pair, Steam R4/L4 convention), PADDLE3/4 = right/left Fn.
            if (b2 and EDGE_BACK_RIGHT != 0) w = w or Gamepad.BTN_PADDLE1
            if (b2 and EDGE_BACK_LEFT != 0) w = w or Gamepad.BTN_PADDLE2
            if (b2 and EDGE_FN_RIGHT != 0) w = w or Gamepad.BTN_PADDLE3
            if (b2 and EDGE_FN_LEFT != 0) w = w or Gamepad.BTN_PADDLE4
        }
        out.buttons = w
        if (len >= l.motionLen) {
            for (i in 0 until 3) out.gyro[i] = cal.gyroToWire(i, i16(r, l.gyro + 2 * i))
            for (i in 0 until 3) out.accel[i] = cal.accelToWire(i, i16(r, l.accel + 2 * i))
        }
        if (len >= l.touchLen) {
            unpackTouch(r, l.touch, out, 0)
            unpackTouch(r, l.touch + 4, out, 1)
        }
        return true
    }

    /** hat nibble (0=N … 7=NW, 8+=neutral) → wire dpad bits — inverse of the host's `hat()`. */
    private fun hatBits(h: Int): Int = when (h) {
        0 -> Gamepad.BTN_DPAD_UP
        1 -> Gamepad.BTN_DPAD_UP or Gamepad.BTN_DPAD_RIGHT
        2 -> Gamepad.BTN_DPAD_RIGHT
        3 -> Gamepad.BTN_DPAD_DOWN or Gamepad.BTN_DPAD_RIGHT
        4 -> Gamepad.BTN_DPAD_DOWN
        5 -> Gamepad.BTN_DPAD_DOWN or Gamepad.BTN_DPAD_LEFT
        6 -> Gamepad.BTN_DPAD_LEFT
        7 -> Gamepad.BTN_DPAD_UP or Gamepad.BTN_DPAD_LEFT
        else -> 0
    }

    /**
     * One 4-byte touch point (shared DS5/DS4 packing — `dualsense_proto::pack_touch`): byte0
     * bit7 = NOT active + contact id in bits 0..6; 12-bit x/y split across bytes 1..3.
     */
    private fun unpackTouch(r: ByteArray, o: Int, out: State, slot: Int) {
        val b0 = u8(r, o)
        out.touchActive[slot] = b0 and 0x80 == 0
        out.touchX[slot] = u8(r, o + 1) or ((u8(r, o + 2) and 0x0F) shl 8)
        out.touchY[slot] = (u8(r, o + 2) shr 4) or (u8(r, o + 3) shl 4)
    }

    private fun u8(r: ByteArray, o: Int): Int = r[o].toInt() and 0xFF

    private fun i16(r: ByteArray, o: Int): Int =
        ((r[o + 1].toInt() shl 8) or (r[o].toInt() and 0xFF)).toShort().toInt()

    // Device stick byte (0..255, centre 0x80, +y down) → wire i16 (+y up) — the exact inverse of
    // the host's `to_u8` mapping (`lx = to_u8(x)`, `ly = 255 - to_u8(y)`).
    private fun stickX(raw: Int): Int = raw * 257 - 32768

    private fun stickY(raw: Int): Int = (255 - raw) * 257 - 32768

    // ---- Output reports ----
    //
    // Every write is valid-flag-selective: only the flagged channel applies, the firmware keeps
    // the rest (the same contract the host's `parse_ds_output` mirrors — an unflagged parse would
    // turn every rumble into a lightbar-off). The DS4 is the exception: its builder writes the
    // full composed motors+LED state each time with both flags, SDL's proven-on-hardware shape.

    // DS5 output report 0x02, report-relative offsets (`dualsense_proto::parse_ds_output`):
    // [1] valid_flag0 (bit0 compat vibration, bit1 haptics select, bit2 R2 block, bit3 L2 block),
    // [2] valid_flag1 (bit0 mic LED, bit2 lightbar, bit4 player LEDs), [3]/[4] motors, [9] mic LED
    // mode, [11..22) R2 effect,
    // [22..33) L2 effect, [39] valid_flag2 (bit1 lightbar-setup enable, bit2 vibration2),
    // [42] lightbar_setup, [44] player LEDs, [45..48) RGB.
    private const val DS5_FLAG0_COMPAT_VIBRATION = 0x01
    private const val DS5_FLAG0_HAPTICS_SELECT = 0x02
    private const val DS5_FLAG0_R2_EFFECT = 0x04
    private const val DS5_FLAG0_L2_EFFECT = 0x08
    private const val DS5_FLAG1_MIC_LED = 0x01
    private const val DS5_FLAG1_LIGHTBAR = 0x04
    private const val DS5_FLAG1_PLAYER_LEDS = 0x10
    private const val DS5_FLAG2_LIGHTBAR_SETUP = 0x02
    private const val DS5_FLAG2_VIBRATION2 = 0x04
    private const val DS5_LIGHTBAR_SETUP_LIGHT_OUT = 0x02

    /** The 11-byte adaptive-trigger effect block length (mode byte + 10 parameters). */
    const val TRIGGER_EFFECT_LEN = 11

    private fun newDs5(model: Model): ByteArray = ByteArray(model.outputSize).also { it[0] = 0x02 }

    /**
     * One-time capture-start report (DS5/Edge): release the firmware's lightbar animation
     * (`LIGHTBAR_SETUP_LIGHT_OUT`) so subsequent host lightbar writes take effect — the same
     * init both hid-playstation and SDL send on open. No-op fields otherwise.
     */
    fun ds5InitReport(model: Model): ByteArray = newDs5(model).also {
        it[39] = DS5_FLAG2_LIGHTBAR_SETUP.toByte()
        it[42] = DS5_LIGHTBAR_SETUP_LIGHT_OUT.toByte()
    }

    /**
     * DS5/Edge rumble at the wire's u16 amplitudes ([low] = heavy/left motor, [high] =
     * light/right — the host parses `[3]` as high and `[4]` as low, mirrored here). Flags both
     * the classic compat-vibration path AND `VIBRATION2` (firmware ≥ 2.24's full-range replot;
     * older firmware ignores the unknown flag2 bit) — the host parser accepts either.
     */
    /**
     * B6: hand the voice coils back to the audio-haptics path.
     *
     * Every [ds5RumbleReport] asserts `HAPTICS_SELECT` (flag0 bit1), which is SDL's
     * "disable audio haptics" bit — the firmware mutes the coils the 0xD1 haptics stream drives.
     * Until now NOTHING ever cleared it again, so a single rumble anywhere in a session left tier-A
     * haptics silent for the rest of that pad's life, with no error and nothing in a log.
     *
     * The undo is a report whose flag0 has BOTH bits clear (SDL's own comment: "Leaving emulated
     * rumble bits off will restore audio haptics"). No other valid flag is set, so nothing else
     * about the pad's state is touched. Mirrors `Ds5Feedback::audio_haptics_packet` on the desktop
     * client, which is the same packet one transport over.
     */
    fun ds5AudioHapticsReport(model: Model): ByteArray = newDs5(model)

    fun ds5RumbleReport(model: Model, low: Int, high: Int): ByteArray = newDs5(model).also {
        it[1] = (DS5_FLAG0_COMPAT_VIBRATION or DS5_FLAG0_HAPTICS_SELECT).toByte()
        it[39] = DS5_FLAG2_VIBRATION2.toByte()
        it[3] = wireAmplitudeToByte(high).toByte()
        it[4] = wireAmplitudeToByte(low).toByte()
    }

    /**
     * DS5/Edge adaptive-trigger effect: [which] 0 = L2, 1 = R2; [effect] is the raw 11-byte
     * trigger block from the wire (`HidOutput::Trigger` — the game's bytes verbatim), copied to
     * the same offsets the host parsed it from ([11..22) R2 / [22..33) L2).
     */
    fun ds5TriggerReport(model: Model, which: Int, effect: ByteArray): ByteArray = newDs5(model).also {
        val at = if (which == 1) 11 else 22
        it[1] = (if (which == 1) DS5_FLAG0_R2_EFFECT else DS5_FLAG0_L2_EFFECT).toByte()
        val n = effect.size.coerceAtMost(TRIGGER_EFFECT_LEN)
        System.arraycopy(effect, 0, it, at, n)
    }

    /** DS5/Edge lightbar RGB. */
    fun ds5LightbarReport(model: Model, r: Int, g: Int, b: Int): ByteArray = newDs5(model).also {
        it[2] = DS5_FLAG1_LIGHTBAR.toByte()
        it[45] = r.toByte()
        it[46] = g.toByte()
        it[47] = b.toByte()
    }

    /** DS5/Edge player-indicator LEDs (low 5 bits, hid-playstation pattern). */
    fun ds5PlayerLedsReport(model: Model, bits: Int): ByteArray = newDs5(model).also {
        it[2] = DS5_FLAG1_PLAYER_LEDS.toByte()
        it[44] = (bits and 0x1F).toByte()
    }

    /** DS5/Edge mic-mute LED: [mode] 0 off, 1 on, 2 pulse. */
    fun ds5MicLedReport(model: Model, mode: Int): ByteArray = newDs5(model).also {
        it[2] = DS5_FLAG1_MIC_LED.toByte()
        it[9] = mode.toByte()
    }

    // DS4 output report 0x05 (32 B), report-relative (`dualshock4_proto::parse_ds4_output`):
    // [1] valid_flag0 (bit0 motors, bit1 LED, bit2 blink), [4] weak/right motor, [5] strong/left,
    // [6..9) RGB, [9]/[10] blink on/off.
    private const val DS4_FLAG0_MOTORS = 0x01
    private const val DS4_FLAG0_LED = 0x02

    /**
     * One full-state DS4 write: motors + lightbar together, both flags set — the composed-state
     * shape SDL uses against real hardware (per-channel selective writes are unproven on DS4
     * firmware, unlike the DS5's). [DsCapture] holds the composition. Blink stays untouched.
     */
    fun ds4Report(low: Int, high: Int, r: Int, g: Int, b: Int): ByteArray =
        ByteArray(Model.DUALSHOCK4.outputSize).also {
            it[0] = 0x05
            it[1] = (DS4_FLAG0_MOTORS or DS4_FLAG0_LED).toByte()
            it[4] = wireAmplitudeToByte(high).toByte()
            it[5] = wireAmplitudeToByte(low).toByte()
            it[6] = r.toByte()
            it[7] = g.toByte()
            it[8] = b.toByte()
        }

}
