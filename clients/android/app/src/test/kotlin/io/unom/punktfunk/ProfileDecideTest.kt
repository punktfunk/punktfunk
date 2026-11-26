package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.ListedProfile
import io.unom.punktfunk.kit.ProfilePick
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** A connect dials a saved pick without asking only when it is remembered, as the console does. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric has no SDK 37 image yet; the app targets 37
class ProfileDecideTest {
    private val context: Context get() = ApplicationProvider.getApplicationContext()
    private val kid = ListedProfile(id = "kid", displayName = "Kid")
    private val mum = ListedProfile(id = "mum", displayName = "Mum")
    private val both = ProfilesAnswer.Listed(listOf(kid, mum))

    private fun seated(store: KnownHostStore, saved: ProfilePick?): KnownHost {
        val kh = store.trust("192.168.1.9", 9777, "Desk", "ab".repeat(32), paired = true)
        HostRecords.savePick(store, kh, saved)
        return store.byId(kh.id)!!
    }

    private val ProfileChoice.dialed: String? get() = (this as ProfileChoice.Dial).id

    /** One connect on [answer]; the picker, if shown, is answered with [pick]. */
    private fun connect(
        store: KnownHostStore,
        kh: KnownHost,
        answer: ProfilesAnswer,
        pick: ProfilePick? = null,
    ): Pair<ProfileChoice, Boolean> {
        var asked = false
        val choice = runBlocking {
            decideProfile(answer, kh.asProfile, store, ClientIdentity("", ""), kh, null, wait = {}) { ask ->
                asked = true
                ask.answer.complete(pick)
            }
        }
        return choice to asked
    }

    @Test
    fun a_saved_pick_without_remember_asks() {
        val store = KnownHostStore(context)
        val kh = seated(store, ProfilePick("kid", "Kid"))
        val (choice, asked) = connect(store, kh, both, ProfilePick("mum", "Mum"))
        assertTrue(asked)
        assertEquals("mum", choice.dialed)
        // Remember off forgets the old pick, as the console's picker does.
        assertNull(store.byId(kh.id)!!.asProfile)
    }

    @Test
    fun a_remembered_pick_dials_without_asking() {
        val store = KnownHostStore(context)
        val kid = ProfilePick("kid", "Kid", remember = true)
        val kh = seated(store, kid)
        val (choice, asked) = connect(store, kh, both)
        assertFalse(asked)
        assertEquals("kid", choice.dialed)
        assertEquals(kid, store.byId(kh.id)!!.asProfile)
    }

    @Test
    fun remember_on_saves_the_pick_and_off_with_nothing_saved_saves_none() {
        val store = KnownHostStore(context)
        val kh = seated(store, null)
        connect(store, kh, both, ProfilePick("mum", "Mum", remember = true))
        assertEquals(ProfilePick("mum", "Mum", remember = true), store.byId(kh.id)!!.asProfile)

        val fresh = store.trust("192.168.1.10", 9777, "Den", "cd".repeat(32), paired = true)
        connect(store, fresh, both, ProfilePick("mum", "Mum"))
        assertNull(store.byId(fresh.id)!!.asProfile)
    }

    @Test
    fun a_failed_answer_dials_only_a_remembered_pick() {
        val store = KnownHostStore(context)
        val failed = ProfilesAnswer.Failed("no answer")
        val plain = seated(store, ProfilePick("kid", "Kid"))
        assertEquals(ProfileChoice.Dial(null), connect(store, plain, failed).first)
        val kept = seated(store, ProfilePick("kid", "Kid", remember = true))
        assertEquals("kid", connect(store, kept, failed).first.dialed)
    }

    @Test
    fun the_hosts_old_seat_of_this_device_stays_remembered() {
        val store = KnownHostStore(context)
        val kh = seated(store, null)
        val old = ProfilesAnswer.Listed(listOf(kid.copy(legacySeat = true), mum))
        val (choice, asked) = connect(store, kh, old)
        assertFalse(asked)
        assertEquals("kid", choice.dialed)
        assertEquals(ProfilePick("kid", "Kid", remember = true), store.byId(kh.id)!!.asProfile)
    }
}
