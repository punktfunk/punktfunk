package io.unom.punktfunk.kit

/**
 * The two conversions every rumble path in this module needs, in one place.
 *
 * [wireAmplitudeToByte] serves both `GamepadFeedback` and `DsDevice`; [unpackRumbleEvent] serves
 * `GamepadFeedback`'s poll loop. One copy of each, so `RumbleWireTest` pins the JNI layout and
 * the amplitude rounding for every caller.
 */

/**
 * Wire amplitude (`0..0xFFFF`) → an 8-bit motor/vibrator level.
 *
 * The high byte, except that a **nonzero command never collapses to zero**: anything below 0x0100
 * would otherwise round to silence, turning a weak-but-real rumble into no rumble at all. 1 is
 * imperceptibly light, but it moves.
 */
internal fun wireAmplitudeToByte(v16: Int): Int {
    val a = (v16 ushr 8) and 0xFF
    return if (v16 != 0 && a == 0) 1 else a
}

/** One effective rumble command, as packed by the native side's `nativeNextRumble`. */
internal data class RumbleCmd(val pad: Int, val low: Int, val high: Int, val backstopMs: Long)

/**
 * Unpack `NativeBridge.nativeNextRumble`'s `jlong`, or null for the timeout/closed sentinel.
 *
 * Layout, mirroring `clients/android/native/src/feedback.rs::pack_rumble`:
 * bits 49..52 = wire pad index, 32..47 = backstop duration (ms), 16..31 = low, 0..15 = high.
 * The pad field is 4 bits because `punktfunk_core::input::MAX_PADS` is 16 — the Rust side has a
 * compile-time assertion tying the two together, so this can't silently start truncating.
 *
 * These are EFFECTIVE commands from the core's shared rumble policy engine: it owns every
 * lease/staleness/close decision and emits explicit zeros, so apply them verbatim —
 * `(0, 0)` = cancel, non-zero = one-shot for the backstop.
 */
internal fun unpackRumbleEvent(ev: Long): RumbleCmd? {
    if (ev < 0L) return null // timeout / closed
    return RumbleCmd(
        pad = ((ev ushr 49) and 0xFL).toInt(),
        low = ((ev ushr 16) and 0xFFFF).toInt(),
        high = (ev and 0xFFFF).toInt(),
        backstopMs = (ev ushr 32) and 0xFFFF,
    )
}
