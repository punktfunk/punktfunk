package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.security.KnownHostStore
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** Both shells forget and pin through [HostRecords], so the console leaves nothing behind either. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric 4.16 has no SDK 37 image yet; the app targets 37
class HostRecordsTest {
    private val context: Context get() = ApplicationProvider.getApplicationContext()

    @Test
    fun forget_drops_the_record_its_title_and_the_default_host() {
        val store = KnownHostStore(context)
        val kh = store.trust("192.168.1.9", 9777, "Desk", "ab".repeat(32), paired = true)
        val other = store.trust("192.168.1.10", 9777, "Sofa", "cd".repeat(32), paired = true)
        LibraryPosition.remember(context, kh.id, "halo")

        val next = HostRecords.forget(context, store, kh, Settings(defaultHost = kh.id))
        assertNull(next!!.defaultHost)
        assertNull(store.byId(kh.id))
        assertNull(LibraryPosition.last(context, kh.id))
        // Another host as the default stands.
        assertNull(HostRecords.forget(context, store, other, Settings(defaultHost = kh.id)))
    }

    @Test
    fun pinning_twice_keeps_one_pin_in_place() {
        val store = KnownHostStore(context)
        val kh = store.trust("192.168.1.9", 9777, "Desk", "ab".repeat(32), paired = true)
            .copy(pinnedPresetIds = listOf("work", "game"))
        HostRecords.setPin(store, kh, "work", pin = true)
        assertEquals(listOf("work", "game"), store.byId(kh.id)!!.pinnedPresetIds)
        HostRecords.togglePin(store, kh, "work")
        assertEquals(listOf("game"), store.byId(kh.id)!!.pinnedPresetIds)
    }
}
