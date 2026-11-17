package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * A dual-boot box answers at one lease with one MAC and a certificate per OS. Trusting the second
 * OS must not overwrite the first one's record, and the advert of whichever OS is up must not read
 * as the host already saved — it would never reach "Discovered" and could not be added at all.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric has no SDK 37 image yet; the app targets 37
class DualBootHostsTest {
    private val context: Context get() = ApplicationProvider.getApplicationContext()
    private val windows = "ab".repeat(32)
    private val linux = "cd".repeat(32)

    private fun advert(fp: String?, host: String = "192.168.1.9") =
        DiscoveredHost(key = "id-$fp", name = "Desk", host = host, port = 9777, fingerprint = fp)

    @Test
    fun a_second_os_at_one_address_is_a_different_host() {
        val store = KnownHostStore(context)
        val saved = store.trust("192.168.1.9", 9777, "Desk (Windows)", windows, paired = true)
        assertFalse(saved.matches(advert(linux)))
        // The pin decides on its own, so a new DHCP lease is still the same host.
        assertTrue(saved.matches(advert(windows.uppercase(), host = "192.168.1.20")))
        // One side unpinned: the address is all there is to go on.
        assertTrue(saved.matches(advert(null)))
    }

    @Test
    fun trusting_the_second_os_leaves_the_first_alone() {
        val store = KnownHostStore(context)
        val first = store.trust("192.168.1.9", 9777, "Desk (Windows)", windows, paired = true)
        val second = store.trust("192.168.1.9", 9777, "Desk (Linux)", linux, paired = true)
        assertEquals(2, store.all().size)
        assertNotNull("the first OS keeps its record", store.byId(first.id))
        assertEquals(windows, store.byId(first.id)?.fpHex)
        assertEquals("Desk (Windows)", store.byId(first.id)?.name)
        assertEquals(linux, store.getByFp(linux)?.fpHex)
        assertEquals(second.id, store.getByFp(linux)?.id)

        // Re-trusting one of them is still an update in place, not a third record.
        store.trust("192.168.1.9", 9777, "Desk (Linux)", linux, paired = true)
        assertEquals(2, store.all().size)
    }

    /**
     * An advert teaches the record it matched, not whichever sibling the address answers with.
     * The OS chain draws the card's icon, so crossing it swaps the two cards' marks.
     */
    @Test
    fun an_advert_teaches_only_its_own_record() {
        val store = KnownHostStore(context)
        val first = store.trust("192.168.1.9", 9777, "Desk (Windows)", windows, paired = true)
        val second = store.trust("192.168.1.9", 9777, "Desk (Linux)", linux, paired = true)
        // BOTH are taught, so a lookup that resolves by address fails whichever sibling it
        // happens to land on — with only one taught, prefs order decides and the test flakes.
        store.learnOs(first, "windows")
        store.learnMgmtPort(first, 47990)
        store.learnOs(second, "linux/fedora/bazzite")
        store.learnMgmtPort(second, 47991)
        assertEquals("windows", store.byId(first.id)?.os)
        assertEquals(47990, store.byId(first.id)?.mgmtPort)
        assertEquals("linux/fedora/bazzite", store.byId(second.id)?.os)
        assertEquals(47991, store.byId(second.id)?.mgmtPort)
    }

    /** An empty fingerprint is not a key — it would match the first unpinned placeholder. */
    @Test
    fun an_unpinned_placeholder_takes_the_first_pin_offered_at_its_address() {
        val store = KnownHostStore(context)
        val placeholder = store.trust("192.168.1.9", 9777, "192.168.1.9", "", paired = false)
        assertNull(store.getByFp(""))
        val pinned = store.trust("192.168.1.9", 9777, "Desk", windows, paired = true)
        assertEquals("the placeholder is the record this pin was waiting for", placeholder.id, pinned.id)
        assertEquals(1, store.all().size)
    }

    /**
     * A dial resolves the record its pin names, or the placeholder waiting for one — never the
     * other OS saved at the same address. Only a bare typed address falls back to the address.
     */
    @Test
    fun a_pin_resolves_its_own_record_never_the_sibling() {
        val store = KnownHostStore(context)
        val first = store.trust("192.168.1.9", 9777, "Desk (Windows)", windows, paired = true)
        assertNull(store.resolve(linux, "192.168.1.9", 9777))
        assertNull(store.resolve("", "192.168.1.9", 9777))
        assertEquals(first.id, store.resolve(null, "192.168.1.9", 9777)?.id)
        assertEquals("a moved lease is the same pin", first.id, store.resolve(windows, "192.168.1.20", 9777)?.id)

        val placeholder = KnownHost("192.168.1.9", 9777, "Desk (Linux)", "", paired = false)
        store.save(placeholder)
        assertEquals(placeholder.id, store.placeholderAt("192.168.1.9", 9777)?.id)
        assertEquals(placeholder.id, store.resolve(linux, "192.168.1.9", 9777)?.id)
        assertEquals(placeholder.id, store.resolve("", "192.168.1.9", 9777)?.id)
    }

    /** A pinned record keeps its name when a dial meant for another card at its address lands on it. */
    @Test
    fun trusting_a_pinned_record_keeps_its_name() {
        val store = KnownHostStore(context)
        val first = store.trust("192.168.1.9", 9777, "Desk (Windows)", windows, paired = false)
        store.trust("192.168.1.9", 9777, "Desk (Linux)", windows, paired = true)
        assertEquals("Desk (Windows)", store.byId(first.id)?.name)
        assertEquals(true, store.byId(first.id)?.paired)
        assertEquals(1, store.all().size)
    }
}
