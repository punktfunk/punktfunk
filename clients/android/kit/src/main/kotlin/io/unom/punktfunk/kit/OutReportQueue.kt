package io.unom.punktfunk.kit

/**
 * The pending output reports for a captured controller: a bounded FIFO whose overflow policy
 * knows which reports may be thrown away and which may not. [T] is what one write carries —
 * the bytes on USB, the bytes and their characteristic on BLE.
 *
 * The queue exists because only one thread may drive a connection's `UsbRequest`s, so writes from
 * the feedback threads are handed to the reader thread rather than submitted directly — and on
 * BLE because Android takes one GATT operation per connection at a time. It has to
 * be bounded — a stalled or unplugged device would otherwise grow it without limit — and the
 * question is what to discard when it fills.
 *
 * Rumble is *level-styled* — the host renews it about every 120 ms, so a dropped frame is soon
 * replaced. Everything else is a **one-shot**: a lightbar colour, a player-LED mask and an
 * adaptive-trigger effect are sent on change and renewed only about once a second. Dropping one
 * leaves the pad wrong until that renewal.
 *
 * So eviction is driven by an explicit [key] supplied by the caller, not by inspecting the bytes.
 * That distinction cannot be recovered from the report itself: every DualSense output report
 * carries the *same* report id and differs only in its `valid_flag` bytes, so an id-keyed policy
 * would let a rumble supersede a lightbar.
 *
 * Two rules:
 *  - A report offered with a coalescing key **replaces** the pending report with that key, in
 *    place. A burst of rumble collapses to its latest value and never displaces anything else.
 *  - Only when the queue is full does anything get dropped, and then the oldest *coalescable*
 *    report goes first. A one-shot is discarded only if the queue is full of nothing but
 *    one-shots — which needs [cap] distinct one-shots outstanding, far beyond what a real pad
 *    produces.
 *
 * Thread-safe: offered by the feedback threads, drained by the reader thread.
 */
internal class OutReportQueue<T>(private val cap: Int = CAP) {
    private class Entry<T>(val key: Int, val data: T)

    private val items = ArrayDeque<Entry<T>>()

    /**
     * Queue [data] for submission. [key] is [NO_COALESCE] for a one-shot, or a caller-chosen
     * constant identifying a level-styled stream whose newer values supersede older ones.
     *
     * Returns false only if the report had to be dropped outright — the caller can then treat the
     * write as failed rather than assuming it is on its way.
     */
    fun offer(data: T, key: Int = NO_COALESCE): Boolean = synchronized(items) {
        if (key != NO_COALESCE) {
            val at = items.indexOfFirst { it.key == key }
            if (at >= 0) {
                // Supersede in place: keeping the queue position stops a fast rumble stream from
                // repeatedly jumping the one-shots queued ahead of it.
                items[at] = Entry(key, data)
                return true
            }
        }
        if (items.size >= cap) {
            val victim = items.indexOfFirst { it.key != NO_COALESCE }
            if (victim >= 0) {
                items.removeAt(victim)
            } else if (key != NO_COALESCE) {
                // Nothing coalescable to sacrifice and this report is itself replaceable — drop it
                // rather than a one-shot that will never come again.
                return false
            } else {
                items.removeFirst()
            }
        }
        items.addLast(Entry(key, data))
        return true
    }

    /** The next report to submit, or null when nothing is pending. */
    fun poll(): T? = synchronized(items) { items.removeFirstOrNull()?.data }

    fun clear() = synchronized(items) { items.clear() }

    val size: Int get() = synchronized(items) { items.size }

    companion object {
        /** This report is a one-shot: never superseded, evicted only as a last resort. */
        const val NO_COALESCE = 0

        /** Motor levels — re-sent continuously, so only the newest is worth keeping. */
        const val KEY_RUMBLE = 1

        /** Deep enough to absorb a burst, small enough that a stalled device cannot bloat us. */
        const val CAP = 32
    }
}
