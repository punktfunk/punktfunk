package io.unom.punktfunk.kit.security

import io.unom.punktfunk.kit.R
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class IdentityHolderTest {
    private val identity = ClientIdentity("cert", "key")

    @Test
    fun concurrentCallersShareOneObtain() {
        val obtains = AtomicInteger()
        val gate = CountDownLatch(1)
        val holder = IdentityHolder({
            obtains.incrementAndGet()
            gate.await()
            identity
        }, onFailure = {})
        val first = holder.ensure()
        val second = holder.ensure()
        gate.countDown()
        assertSame(identity, first.get())
        assertSame(identity, second.get())
        assertSame(identity, holder.await())
        assertEquals(1, obtains.get())
    }

    /** A wedged keystore surfaces as a failure within the bound, and the next ask retries it. */
    @Test
    fun aWedgedObtainFailsInsideTheBoundAndRetries() {
        val wedge = CountDownLatch(1)
        val calls = AtomicInteger()
        val holder = IdentityHolder({
            if (calls.incrementAndGet() == 1) wedge.await()
            identity
        }, timeoutMs = 500, onFailure = {}, words = { "message $it" })
        assertEquals("message ${R.string.android_identity_not_ready}", holder.blockedMessage())
        assertNull(holder.await())
        assertTrue(holder.failed && holder.settled)
        assertEquals("message ${R.string.android_identity_broken}", holder.blockedMessage())
        assertSame(identity, holder.ensure().get(1, TimeUnit.SECONDS))
        assertSame(identity, holder.current)
        assertFalse(holder.failed)
        wedge.countDown()
    }

    @Test
    fun aThrowingObtainReportsItsCause() {
        val failure = IdentityUnrecoverableException("store unreadable", null)
        var reported: Throwable? = null
        val holder = IdentityHolder({ throw failure }, onFailure = { reported = it })
        assertNull(holder.await())
        assertSame(failure, reported)
        assertTrue(holder.failed)
    }
}
