package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.security.KnownHostStore
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** Reset is a fresh install's settings; the default host and every host record stay. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric 4.16 has no SDK 37 image yet; the app targets 37
class SettingsResetTest {
    private val context: Context get() = ApplicationProvider.getApplicationContext()

    @Test
    fun aResetIsAFreshInstallOnThisKindOfDevice() {
        val changed = Settings(
            width = 2560,
            height = 1440,
            codec = "av1",
            hdrEnabled = false,
            gamepadUiEnabled = false,
            showAdvanced = true,
            reduceUiResolution = false,
            defaultHost = "desk",
        )
        assertEquals(Settings(defaultHost = "desk"), changed.reset(tv = false))
        assertEquals(Settings(reduceUiResolution = true, defaultHost = "desk"), changed.reset(tv = true))
    }

    @Test
    fun aResetLeavesPairingAlone() {
        val hosts = KnownHostStore(context)
        val kh = hosts.trust("192.168.1.9", 9777, "Desk", "ab".repeat(32), paired = true)
        val store = SettingsStore(context, tv = false)
        store.save(Settings(codec = "av1", micEnabled = true, defaultHost = kh.id))

        store.save(store.load().reset(tv = false))

        assertEquals(Settings(defaultHost = kh.id), store.load())
        assertEquals(kh, hosts.byId(kh.id))
    }
}
