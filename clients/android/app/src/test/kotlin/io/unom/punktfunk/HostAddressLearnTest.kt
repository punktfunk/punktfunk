package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.security.KnownHostStore
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * A host that answered the probe somewhere else has moved, and every dial reads the saved
 * address — so the record follows it, by fingerprint, keeping everything the user set on it.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric has no SDK 37 image yet; the app targets 37
class HostAddressLearnTest {
    private val context: Context get() = ApplicationProvider.getApplicationContext()
    private val fp = "ab".repeat(32)

    @Test
    fun a_pinned_host_follows_the_address_that_answered() {
        val store = KnownHostStore(context)
        val saved = store.trust("192.168.1.9", 9777, "Desk", fp, paired = true)
        store.save(saved.copy(mac = listOf("aa:bb:cc:dd:ee:ff"), mgmtPort = 47991))
        assertTrue(store.learnAddress(fp.uppercase(), "192.168.1.20", 9777))
        val moved = store.byId(saved.id)!!
        assertEquals("192.168.1.20", moved.address)
        assertEquals(listOf("192.168.1.9"), moved.prevAddresses)
        assertEquals(listOf("aa:bb:cc:dd:ee:ff"), moved.mac)
        assertEquals(47991, moved.mgmtPort)
        assertEquals(1, store.all().size)
        // Unchanged is a no-op; an unpinned record is named by its address and never moves.
        assertFalse(store.learnAddress(fp, "192.168.1.20", 9777))
        store.trust("192.168.1.30", 9777, "Sofa", "", paired = false)
        assertFalse(store.learnAddress("", "192.168.1.31", 9777))
    }

    /** A kept address is the player's: neither a probe elsewhere nor a pairing there moves it. */
    @Test
    fun a_kept_address_never_follows_the_host() {
        val store = KnownHostStore(context)
        val saved = store.trust("10.10.0.2", 9777, "Desk", fp, paired = true)
        store.save(saved.copy(keepAddress = true))
        assertFalse(store.learnAddress(fp, "192.168.1.9", 9777))
        assertEquals("10.10.0.2", store.byId(saved.id)!!.address)
        assertTrue(store.byId(saved.id)!!.prevAddresses.isEmpty())
        val repaired = store.trust("192.168.1.9", 9777, "Desk", fp, paired = true)
        assertEquals(saved.id, repaired.id)
        assertEquals("10.10.0.2", repaired.address)
        assertTrue(repaired.keepAddress)
        assertEquals(1, store.all().size)
        // A placeholder that keeps its address keeps it through the pairing that pins it.
        val typed = store.trust("10.10.0.3", 9777, "10.10.0.3", "", paired = false)
        store.save(typed.copy(keepAddress = true))
        val pinned = store.trust("10.10.0.3", 9777, "Sofa", "cd".repeat(32), paired = true)
        assertEquals(typed.id, pinned.id)
        assertTrue(pinned.keepAddress)
    }

    /** The addresses a host left: newest first, no repeats, up to `PREV_ADDRESSES_MAX`. */
    @Test
    fun a_moved_host_remembers_the_addresses_it_left() {
        val store = KnownHostStore(context)
        val saved = store.trust("100.64.0.7", 9777, "Desk", fp, paired = true)
        store.learnAddress(fp, "192.168.1.9", 9777)
        store.learnAddress(fp, "100.64.0.7", 9777)
        assertEquals(listOf("192.168.1.9"), store.byId(saved.id)!!.prevAddresses)
        for (a in listOf("10.0.0.1", "10.0.0.2", "10.0.0.3")) store.learnAddress(fp, a, 9777)
        assertEquals(listOf("10.0.0.2", "10.0.0.1", "100.64.0.7"), store.byId(saved.id)!!.prevAddresses)
    }
}
