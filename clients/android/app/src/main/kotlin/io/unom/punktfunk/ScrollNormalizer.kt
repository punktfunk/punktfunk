package io.unom.punktfunk

/**
 * The normalized-scroll wire vocabulary (`punktfunk_core::input::scroll`) in Kotlin: a delta is
 * signed Q24.8 in the source's own unit — 120-per-detent for a wheel (and for an unknown source,
 * which the wire prices as a wheel), device-independent pixels for everything that measures
 * distance. Positive is up on the vertical axis, right on the horizontal. `phase` carries the
 * gesture boundary; Android's ACTION_SCROLL reports none, so hardware scroll is PHASE_NONE.
 */
internal object ScrollWire {
    const val AXIS_VERTICAL = 0
    const val AXIS_HORIZONTAL = 1

    const val SOURCE_UNKNOWN = 0
    const val SOURCE_WHEEL = 1
    const val SOURCE_FINGER = 2
    const val SOURCE_CONTINUOUS = 3
    const val SOURCE_TOUCH = 4
    const val SOURCE_CONTROLLER = 5

    const val PHASE_NONE = 0
    const val PHASE_BEGIN = 1
    const val PHASE_UPDATE = 2
    const val PHASE_END = 3
    const val PHASE_CANCEL = 4
    const val PHASE_MOMENTUM_BEGIN = 5
    const val PHASE_MOMENTUM = 6
    const val PHASE_MOMENTUM_END = 7

    /** Q24.8 fixed-point scale of a wire delta. */
    const val SCALE = 256.0

    fun isWheel(source: Int) = source == SOURCE_UNKNOWN || source == SOURCE_WHEEL

    fun isStop(phase: Int) =
        phase == PHASE_END || phase == PHASE_CANCEL || phase == PHASE_MOMENTUM_END

    fun isMomentum(phase: Int) =
        phase == PHASE_MOMENTUM_BEGIN || phase == PHASE_MOMENTUM || phase == PHASE_MOMENTUM_END

    /** A legal wire source/phase pair — the same rules `ScrollEvent::from_event` enforces. */
    fun validCombo(source: Int, phase: Int): Boolean = when {
        source !in SOURCE_UNKNOWN..SOURCE_CONTROLLER -> false
        phase !in PHASE_NONE..PHASE_MOMENTUM_END -> false
        // Detent counters carry no gesture state; kinetic needs a surface that can glide.
        isWheel(source) && phase != PHASE_NONE -> false
        isMomentum(phase) &&
            source != SOURCE_FINGER && source != SOURCE_CONTINUOUS && source != SOURCE_TOUCH -> false
        else -> true
    }
}

/** One quantized scroll event, ready for `NativeBridge.nativeSendNormalizedScroll`. */
internal data class NormalizedScroll(val axis: Int, val delta: Int, val source: Int, val phase: Int)

/**
 * Floating-point capture deltas → wire Q24.8, the Kotlin twin of the core's `ScrollAccumulator`:
 * the unsent fraction rides per axis, a source switch or a gesture boundary clears it, and a
 * rejected call changes no state. `delta` arrives in the source's own unit (v120 or DIP).
 */
internal class ScrollNormalizer {
    private val rem = doubleArrayOf(0.0, 0.0)
    private val last = intArrayOf(-1, -1)

    /**
     * Quantize [delta] into a wire event, or null on an invalid axis, a non-finite delta, a
     * nonzero stop, an illegal source/phase pair, or a movement that quantizes to zero.
     * Boundary phases still emit at zero distance so a gesture never loses its close.
     */
    fun event(source: Int, phase: Int, axis: Int, delta: Double): NormalizedScroll? {
        if (axis !in 0..1 || !delta.isFinite()) return null
        val stop = ScrollWire.isStop(phase)
        // A stop carries no distance — reject before the residue can swallow it.
        if (stop && delta != 0.0) return null
        if (!ScrollWire.validCombo(source, phase)) return null
        // A boundary restarts the residue as surely as a source switch: a missed stop cannot
        // leak last gesture's fraction into the new one.
        val boundary = stop || phase == ScrollWire.PHASE_BEGIN || phase == ScrollWire.PHASE_MOMENTUM_BEGIN
        if (last[axis] != source || boundary) rem[axis] = 0.0
        last[axis] = source
        val q = if (stop) {
            0
        } else {
            // Clamp before the split so a huge delta saturates instead of leaving an
            // unreachable residue; toInt truncates toward zero, keeping the sign.
            val total = (delta * ScrollWire.SCALE + rem[axis])
                .coerceIn(Int.MIN_VALUE.toDouble(), Int.MAX_VALUE.toDouble())
            val out = total.toInt()
            rem[axis] = total - out
            out
        }
        if (q == 0 && !boundary) return null
        return NormalizedScroll(axis, q, source, phase)
    }

    /**
     * One ACTION_SCROLL event → wire deltas on both axes. [source] is the already-mapped wire
     * source ([ScrollWire.SOURCE_FINGER] measures distance: the axis counts scale to pixels by
     * the OS scroll factors, then to DIP by [density]; anything else counts detents → v120).
     * ACTION_SCROLL reports no gesture boundary, so the phase is always PHASE_NONE.
     */
    fun wheel(
        rawV: Double,
        rawH: Double,
        source: Int,
        scrollFactorV: Double,
        scrollFactorH: Double,
        density: Double,
    ): List<NormalizedScroll> {
        val (v, h) = if (source == ScrollWire.SOURCE_FINGER) {
            (rawV * scrollFactorV / density) to (rawH * scrollFactorH / density)
        } else {
            (rawV * 120.0) to (rawH * 120.0)
        }
        return listOfNotNull(
            event(source, ScrollWire.PHASE_NONE, ScrollWire.AXIS_VERTICAL, v),
            event(source, ScrollWire.PHASE_NONE, ScrollWire.AXIS_HORIZONTAL, h),
        )
    }

    /**
     * A touchpad's finger travel in window pixels → Finger deltas in DIP. The content follows
     * the fingers, as on a touchscreen: fingers down scrolls up, fingers right scrolls left.
     */
    fun travel(dx: Float, dy: Float, density: Float): List<NormalizedScroll> = listOfNotNull(
        event(ScrollWire.SOURCE_FINGER, ScrollWire.PHASE_NONE, ScrollWire.AXIS_VERTICAL, dy / density.toDouble()),
        event(ScrollWire.SOURCE_FINGER, ScrollWire.PHASE_NONE, ScrollWire.AXIS_HORIZONTAL, -dx / density.toDouble()),
    )
}
