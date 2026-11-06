package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.discovery.WakeLoop
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHostStore
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withContext
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** Both shells dial, pin and wake through [HostServices]; here it runs over a fake native layer. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric 4.16 has no SDK 37 image yet; the app targets 37
class HostServicesTest {
    private val context: Context get() = ApplicationProvider.getApplicationContext()
    private val store by lazy { KnownHostStore(context) }
    private val native = FakeNative()
    private val services by lazy { HostServices(store, native) }
    private val me = ClientIdentity("cert", "key")

    @After
    fun releaseTheGate() = SessionGate.release()

    private fun spec(pin: String = "") = DialSpec(Settings(), me, ADDR, PORT, pin, launch = null, dialer = "test")

    @Test
    fun a_dial_that_lands_hands_over_the_session() = runBlocking<Unit> {
        val kh = store.trust(ADDR, PORT, "Desk", FP, paired = true)
        native.handles += 7L
        native.mgmtPort = 47991
        val dialed = services.dial(spec(FP), kh, "Desk") as HostServices.Connected
        assertEquals(7L, dialed.session.handle)
        assertEquals(kh.id, dialed.session.hostId)
        assertFalse(dialed.pinned)
        assertEquals(47991, store.byId(kh.id)?.mgmtPort)
        // A first dial to a host never saved keeps what it presented, unpaired.
        native.handles += 8L
        native.fingerprint = OTHER
        val first = services.dial(spec(), null, "Sofa") as HostServices.Connected
        assertTrue(first.pinned)
        assertEquals("Sofa", first.record?.name)
        assertFalse(store.getByFp(OTHER)!!.paired)
    }

    @Test
    fun a_host_that_asks_for_a_pin_refuses_the_dial_until_paired() = runBlocking<Unit> {
        native.handles += 0L
        native.tokens += "identity-required"
        native.fingerprint = FP
        val refused = services.dial(spec(), null, "Desk") as HostServices.Failed
        assertEquals("identity-required", refused.token)
        // A typed refusal means the host is awake: no wake for it.
        assertFalse(refused.unreachable)
        assertTrue(store.all().isEmpty())
        native.pairAnswer = FP
        val paired = services.pair(ADDR, PORT, me, "1234", "Phone") as HostServices.Paired.Ok
        assertTrue(services.trustPaired(ADDR, PORT, "Desk", paired.fp).paired)
        assertTrue(store.getByFp(FP)!!.paired)
    }

    @Test
    fun a_rejected_trust_saves_nothing() = runBlocking<Unit> {
        val kh = store.trust(ADDR, PORT, "Desk", FP, paired = true)
        native.handles += 0L
        native.tokens += "crypto"
        native.fingerprint = OTHER
        val refused = services.dial(spec(FP), kh, "Desk") as HostServices.Failed
        assertEquals("crypto", refused.token)
        assertFalse(refused.unreachable)
        // An operator's no for a request is the same: nothing pinned.
        native.tokens += "denied"
        assertEquals(HostServices.Paired.Failed("denied"), services.requestAccess(ADDR, PORT, me, "", "Phone"))
        assertEquals(1, store.all().size)
        assertEquals(FP, store.byId(kh.id)?.fpHex)
    }

    @Test
    fun a_wake_that_answers_stops_waiting() {
        val kh = store.trust(ADDR, PORT, "Desk", FP, paired = true).copy(mac = listOf(MAC))
        native.answers = { n -> if (n >= 3) FP else null }
        val ticks = mutableListOf<Triple<Int, Boolean, Boolean>>()
        val up = services.wake(services.wakes.next(), kh.mac, kh.address, isOnline = { services.answers(kh, null, 900) }) { s, t, o ->
            ticks += Triple(s, t, o)
        }
        assertTrue(up)
        assertEquals(listOf(Triple(0, false, false), Triple(1, false, false), Triple(2, false, true)), ticks)
        assertEquals(1, native.packets)
        assertEquals(listOf("$ADDR:$PORT"), native.probed.distinct())
    }

    @Test
    fun a_wake_that_times_out_gives_up_at_the_budget() {
        val kh = store.trust(ADDR, PORT, "Desk", FP, paired = true).copy(mac = listOf(MAC))
        var last: Triple<Int, Boolean, Boolean>? = null
        val up = services.wake(services.wakes.next(), kh.mac, kh.address, isOnline = { services.answers(kh, null, 900) }) { s, t, o ->
            last = Triple(s, t, o)
        }
        assertFalse(up)
        assertEquals(Triple(WakeLoop.TIMEOUT_S, true, false), last)
        assertEquals(15, native.packets)
        // A newer wake, or the console's CancelWake, stops an older one before it sends.
        val older = services.wakes.next()
        services.wakes.next()
        assertFalse(services.wake(older, kh.mac, kh.address, isOnline = { false }) { _, _, _ -> error("ticked") })
        assertEquals(15, native.packets)
    }

    @Test
    fun a_dial_cancelled_mid_flight_closes_its_late_session() = runBlocking<Unit> {
        native.fingerprint = FP
        native.handles += 9L
        val wire = native.holdNextConnect()
        val ticket = HostServices.Ticket()
        val dialing = async(Dispatchers.Default) { services.dial(spec(), null, "Desk", ticket = ticket) }
        native.awaitOnWire()
        ticket.cancel()
        wire.countDown()
        assertEquals(HostServices.Cancelled, dialing.await())
        assertEquals(listOf(9L), native.closed)
        // Nothing pinned, and the gate is free for the next dial.
        assertTrue(store.all().isEmpty())
        assertTrue(SessionGate.take())
    }

    @Test
    fun two_dials_in_a_row_open_one_session_at_a_time() = runBlocking<Unit> {
        val kh = store.trust(ADDR, PORT, "Desk", FP, paired = true)
        native.handles += listOf(10L, 11L)
        val wire = native.holdNextConnect()
        val first = async(Dispatchers.Default) { services.dial(spec(FP), kh, "Desk") }
        native.awaitOnWire()
        // A second dial while the first is out never reaches the host.
        val second = services.dial(spec(FP), kh, "Desk") as HostServices.Failed
        assertTrue(second.unreachable)
        assertEquals(1, native.connects)
        wire.countDown()
        assertEquals(10L, (first.await() as HostServices.Connected).session.handle)
        // Once it is back, the next dial goes out.
        assertEquals(11L, (services.dial(spec(FP), kh, "Desk") as HostServices.Connected).session.handle)
        assertEquals(2, native.connects)
    }

    @Test
    fun a_woken_host_that_refuses_setup_is_dialed_again() = runBlocking<Unit> {
        val kh = store.trust(ADDR, PORT, "Desk", FP, paired = true)
        native.sleep(500)
        native.handles += listOf(0L, 0L, 12L)
        native.tokens += listOf("setup-failed", "setup-failed")
        native.wakeWaits += listOf(1L, 1L)
        val dialed = services.dial(spec(FP), kh, "Desk") as HostServices.Connected
        assertEquals(12L, dialed.session.handle)
        assertEquals(3, native.connects)
        // The window runs from the first refusal and rides into the session's redial.
        assertEquals(listOf(-1L, 0L), native.wakeAsks)
        assertEquals(500L, dialed.session.redial?.wakeSince)
        native.wakeWaits += 2L
        assertEquals(2L, services.wakeRedial(dialed.session)?.second)
        // Once the core says the window shut, the refusal shows.
        native.handles += 0L
        native.tokens += "setup-failed"
        assertEquals("setup-failed", (services.dial(spec(FP), kh, "Desk") as HostServices.Failed).token)
    }

    @Test
    fun an_unlock_rides_only_the_first_dial_of_a_woken_host() = runBlocking<Unit> {
        val kh = store.trust(ADDR, PORT, "Desk", FP, paired = true)
        native.handles += listOf(0L, 13L)
        native.tokens += "setup-failed"
        native.wakeWaits += 1L
        val unlock = Unlock(UNLOCK_PASSWORD, byteArrayOf(1, 2, 3))
        services.dial(spec(FP), kh, "Desk", unlock = unlock) as HostServices.Connected
        assertEquals(listOf(listOf<Byte>(1, 2, 3), null), native.unlocks.map { it?.toList() })
        // A dial the gate refuses never reaches the host, and its secret is gone.
        assertTrue(SessionGate.take())
        val refused = Unlock(UNLOCK_PASSWORD, byteArrayOf(4, 5))
        services.dial(spec(FP), kh, "Desk", unlock = refused) as HostServices.Failed
        assertEquals(2, native.connects)
        assertTrue(refused.secret.all { it == 0.toByte() })
    }

    /** The native layer, scripted: each connect takes the next of [handles]. */
    private class FakeNative : HostNative {
        val handles = ArrayDeque<Long>()
        val tokens = ArrayDeque<String>()
        var fingerprint = ""
        var mgmtPort = 0
        var pairAnswer = ""

        /** The fingerprint the [n]th probe hears, counted from 1. */
        var answers: (Int) -> String? = { null }
        val probed = mutableListOf<String>()
        val closed = mutableListOf<Long>()

        @Volatile var connects = 0
        var packets = 0
        private var clock = 0L
        private var wire: CountDownLatch? = null
        private val onWire = CountDownLatch(1)

        /** The next connect waits on the returned latch: the dial is out until it counts down. */
        fun holdNextConnect() = CountDownLatch(1).also { wire = it }

        fun awaitOnWire() = assertTrue(onWire.await(5, TimeUnit.SECONDS))

        /** What each connect carried; a copy of the secret, taken before the dial wipes it. */
        val unlocks = mutableListOf<ByteArray?>()

        override suspend fun connect(spec: DialSpec, unlock: Unlock?): Long {
            connects++
            unlocks += unlock?.secret?.copyOf()
            wire?.let { held ->
                wire = null
                onWire.countDown()
                withContext(Dispatchers.IO) { held.await(5, TimeUnit.SECONDS) }
            }
            return handles.removeFirst()
        }

        override fun lastError(): String = tokens.removeFirstOrNull() ?: ""
        override fun close(handle: Long) {
            closed += handle
        }

        override fun fingerprint(handle: Long): String = fingerprint
        override fun mgmtPort(handle: Long): Int = mgmtPort
        override fun probe(address: String, port: Int, timeoutMs: Int): String? {
            probed += "$address:$port"
            return answers(probed.size)
        }

        override fun wakeOnLan(macs: List<String>, lastIp: String) {
            packets++
        }

        override fun pair(address: String, port: Int, identity: ClientIdentity, pin: String, deviceName: String) =
            pairAnswer

        override fun requestAccess(
            address: String,
            port: Int,
            identity: ClientIdentity,
            pinHex: String,
            deviceName: String,
        ) = ""

        /** The core's woken-host answers, in order; -1 once spent. */
        val wakeWaits = ArrayDeque<Long>()
        val wakeAsks = mutableListOf<Long>()
        override fun wakeRedialDelayMs(autoWake: Boolean, token: String, sinceFirstMs: Long): Long {
            wakeAsks += sinceFirstMs
            return wakeWaits.removeFirstOrNull() ?: -1
        }

        override fun sessionWakeRedialDelayMs(handle: Long, autoWake: Boolean, sinceFirstMs: Long) =
            wakeRedialDelayMs(autoWake, "", sinceFirstMs)

        override fun elapsedMs(): Long = clock
        override fun sleep(ms: Long) {
            clock += ms
        }
    }

    private companion object {
        const val ADDR = "192.168.1.9"
        const val PORT = 9777
        const val MAC = "aa:bb:cc:dd:ee:ff"
        val FP = "ab".repeat(32)
        val OTHER = "cd".repeat(32)
    }
}
