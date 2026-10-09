package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.ProfilePick
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** The touch home and the library grant the same one redial, and clear a stale pick alike. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric 4.16 has no SDK 37 image yet; the app targets 37
class ProfileRetryTest {
    private val context: Context get() = ApplicationProvider.getApplicationContext()
    private val kid = ProfilePick("kid", "Kid")

    private fun seated(store: KnownHostStore): KnownHost {
        val kh = store.trust("192.168.1.9", 9777, "Desk", "ab".repeat(32), paired = true)
        HostRecords.savePick(store, kh, kid)
        return store.byId(kh.id)!!
    }

    private fun retry(
        store: KnownHostStore,
        token: String,
        record: KnownHost?,
        id: String?,
        redial: Boolean,
        stillThere: Boolean,
    ): Pair<Boolean, Int> {
        var asked = 0
        val again = runBlocking {
            ProfileRetry.afterRefusal(token, record, ProfileChoice.Dial(id), redial, store) { _, _ ->
                asked++
                stillThere
            }
        }
        return again to asked
    }

    @Test
    fun a_stale_seat_of_a_listed_profile_earns_one_redial() {
        val store = KnownHostStore(context)
        val kh = seated(store)
        assertEquals(true to 1, retry(store, "profile-unknown", kh, "kid", redial = false, stillThere = true))
        assertEquals(kid, store.byId(kh.id)!!.asProfile)
    }

    @Test
    fun the_redial_itself_earns_none_and_clears_the_pick() {
        val store = KnownHostStore(context)
        val kh = seated(store)
        assertEquals(false to 0, retry(store, "profile-unknown", kh, "kid", redial = true, stillThere = true))
        assertNull(store.byId(kh.id)!!.asProfile)
    }

    @Test
    fun a_profile_gone_from_the_host_clears_the_pick() {
        val store = KnownHostStore(context)
        val kh = seated(store)
        assertFalse(retry(store, "profile-unknown", kh, "kid", redial = false, stillThere = false).first)
        assertNull(store.byId(kh.id)!!.asProfile)
        // A dial that sent no profile has nothing to ask about.
        val other = seated(store)
        assertEquals(false to 0, retry(store, "profile-unknown", other, null, redial = false, stillThere = true))
        assertNull(store.byId(other.id)!!.asProfile)
    }

    @Test
    fun another_refusal_leaves_the_pick_alone() {
        val store = KnownHostStore(context)
        val kh = seated(store)
        assertEquals(false to 0, retry(store, "busy", kh, "kid", redial = false, stillThere = true))
        assertEquals(kid, store.byId(kh.id)!!.asProfile)
        // No record: nothing to redial from, nothing to clear.
        assertEquals(false to 0, retry(store, "profile-unknown", null, "kid", redial = false, stillThere = true))
    }
}
