package io.unom.punktfunk.kit

/**
 * The hand-off of one claim's motion calibration, from the thread that reads it off the pad to the
 * link thread that scales every input report with it.
 *
 * [DsCapture] reads a captured Sony pad's calibration feature report **off** the claiming thread —
 * it is a blocking EP0 control transfer and the claim runs on the UI's thread — so the value lands
 * a moment after the capture goes live. Reports in that gap are scaled by
 * [DsDevice.MotionCal.NOMINAL] and forwarded like any other ([effective]): gyro unscaled,
 * acceleration on the nominal range, which nobody can feel, whereas a pad that ignores its
 * buttons until an EP0 read comes back is very obvious.
 *
 * What the hand-off is actually for is the two things that gap must NOT do, neither of which a
 * plain field gives:
 *
 *  - **Fall back to the previous pad's numbers instead of the nominal ones.** Calibration is per
 *    unit, so the last controller's scale factors are simply wrong for this one — more wrong, in
 *    general, than the nominal constants. [begin] forgets them, which is what makes the gap
 *    nominal rather than inherited.
 *  - **Let a read that outlived its claim publish.** An unplug, a [DsCapture.stop] and a fast
 *    re-claim can all land while a read is in flight; [publish] only accepts a value whose token is
 *    still the live claim's, so a straggler can never scale a pad it never read.
 *
 * Thread-safe: [begin] by the claiming thread, [end] by it or the link's close handler, [publish]
 * by the reading thread, [effective] by the link thread.
 */
internal class MotionCalHandoff {
    /** Handed out by [begin] and burned by [end] — never reused, so a straggler can't match. */
    private var token = 0

    @Volatile private var cal: DsDevice.MotionCal? = null

    /**
     * The calibration to scale the next report with: the live claim's own, or the nominal fallback
     * while its read is still in flight. Never null — a report is always forwarded, never held
     * back waiting for a control transfer.
     */
    val effective: DsDevice.MotionCal get() = cal ?: DsDevice.MotionCal.NOMINAL

    /** Open a claim: forget the previous pad's calibration, and take this claim's token. */
    @Synchronized
    fun begin(): Int {
        cal = null
        return ++token
    }

    /** End the live claim. Nothing read under an older token can land after this. */
    @Synchronized
    fun end() {
        cal = null
        token++
    }

    /** Publish [value] if [claim] is still the live claim; returns whether it landed. */
    @Synchronized
    fun publish(claim: Int, value: DsDevice.MotionCal): Boolean {
        if (claim != token) return false
        cal = value
        return true
    }
}
