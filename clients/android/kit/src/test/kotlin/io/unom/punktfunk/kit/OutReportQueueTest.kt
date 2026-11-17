package io.unom.punktfunk.kit

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The pending-OUT queue's overflow policy: rumble is re-sent continuously and may be thrown away,
 * while a lightbar/player-LED/trigger report is sent once and never repeated, so overflow drops
 * rumble first.
 */
class OutReportQueueTest {
    /** A report carrying a 0..255 marker so a test can tell which one came back out. */
    private fun report(marker: Int) = byteArrayOf(0x02, marker.toByte())

    // Masked: the marker rides in a Byte, and Byte.toInt() sign-extends.
    private fun drain(q: OutReportQueue<ByteArray>): List<Int> =
        generateSequence { q.poll() }.map { it[1].toInt() and 0xFF }.toList()

    @Test
    fun `rumble supersedes the pending rumble instead of queueing another`() {
        val q = OutReportQueue<ByteArray>()
        assertTrue(q.offer(report(1), OutReportQueue.KEY_RUMBLE))
        assertTrue(q.offer(report(2), OutReportQueue.KEY_RUMBLE))
        assertTrue(q.offer(report(3), OutReportQueue.KEY_RUMBLE))
        assertEquals("a rumble burst must collapse to one entry", 1, q.size)
        assertArrayEquals(report(3), q.poll())
        assertNull(q.poll())
    }

    @Test
    fun `superseding keeps the queue position so a rumble stream cannot jump one-shots`() {
        val q = OutReportQueue<ByteArray>()
        q.offer(report(1), OutReportQueue.KEY_RUMBLE)
        q.offer(report(10)) // a one-shot queued behind it
        q.offer(report(2), OutReportQueue.KEY_RUMBLE)
        // The newer rumble takes the OLD rumble's slot, so the one-shot does not get starved
        // behind an endlessly-renewed entry.
        assertEquals(listOf(2, 10), drain(q))
    }

    @Test
    fun `a full queue sacrifices rumble, never a one-shot`() {
        val q = OutReportQueue<ByteArray>(cap = 4)
        q.offer(report(1), OutReportQueue.KEY_RUMBLE)
        q.offer(report(10))
        q.offer(report(11))
        q.offer(report(12))
        assertEquals(4, q.size)
        // Full: the rumble is the victim, so the one-shots all survive.
        assertTrue(q.offer(report(13)))
        assertEquals(listOf(10, 11, 12, 13), drain(q))
    }

    @Test
    fun `the one-shot the host never repeats survives a rumble storm`() {
        val q = OutReportQueue<ByteArray>(cap = 4)
        // A lightbar colour queued once, then a flood of rumble: the colour stays queued, since
        // the host never repeats it.
        q.offer(report(200)) // lightbar
        repeat(50) { q.offer(report(it), OutReportQueue.KEY_RUMBLE) }
        val out = drain(q)
        assertTrue("the lightbar report must still be queued, got $out", out.contains(200))
        assertEquals("rumble must not have accumulated", listOf(200, 49), out)
    }

    @Test
    fun `a queue full of one-shots refuses a rumble rather than dropping one`() {
        val q = OutReportQueue<ByteArray>(cap = 2)
        q.offer(report(10))
        q.offer(report(11))
        assertFalse(
            "with nothing coalescable to sacrifice, the replaceable report yields",
            q.offer(report(1), OutReportQueue.KEY_RUMBLE),
        )
        assertEquals(listOf(10, 11), drain(q))
    }

    @Test
    fun `only a queue of nothing but one-shots drops one, and it is the oldest`() {
        val q = OutReportQueue<ByteArray>(cap = 2)
        q.offer(report(10))
        q.offer(report(11))
        assertTrue(q.offer(report(12)))
        assertEquals(listOf(11, 12), drain(q))
    }

    @Test
    fun `clear empties the queue`() {
        val q = OutReportQueue<ByteArray>()
        q.offer(report(1), OutReportQueue.KEY_RUMBLE)
        q.offer(report(10))
        q.clear()
        assertEquals(0, q.size)
        assertNull(q.poll())
    }
}
