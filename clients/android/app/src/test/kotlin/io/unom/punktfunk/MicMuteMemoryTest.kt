package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.SessionAccess
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** The mic mute a stream starts from is the one the last toggle left on this device. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric has no SDK 37 image yet; the app targets 37
class MicMuteMemoryTest {
    private val context: Context get() = ApplicationProvider.getApplicationContext()
    private val native = mutableListOf<Boolean>()

    /** What `StreamScreen` builds: the saved mute in, the native flag and the save out. */
    private fun stream() = StreamUi(
        0, intArrayOf(SessionAccess.ALL, 0), StatsVerbosity.entries.first(),
        applyInvertScroll = { true },
        initialMicMuted = MicMuteMemory.muted(context),
        applyMicMuted = { native += it },
        keepMicMuted = { MicMuteMemory.keep(context, it) },
    )

    @Test
    fun aFreshDeviceStartsLiveAndTouchesNothing() {
        val ui = stream().also { it.seed() }
        assertFalse(ui.micMuted)
        assertTrue(native.isEmpty())
    }

    @Test
    fun aMuteReachesTheNextStreamBeforeItsMicOpens() {
        stream().mute(true)
        val next = stream()
        assertTrue("the toggle shows the truth from the first frame", next.micMuted)
        next.seed()
        assertEquals("the native flag is set by the seed, ahead of any capture", listOf(true, true), native)
    }

    @Test
    fun anUnmuteIsRemembered() {
        stream().mute(true)
        stream().mute(false)
        assertFalse(stream().micMuted)
    }
}
