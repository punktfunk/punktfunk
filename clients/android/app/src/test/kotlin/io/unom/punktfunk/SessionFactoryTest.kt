package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.security.KnownHostStore
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** Every shell's dial ends here, so the host's record learns what the session told it. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric has no SDK 37 image yet; the app targets 37
class SessionFactoryTest {
    private val context: Context get() = ApplicationProvider.getApplicationContext()

    @Test
    fun a_dial_saves_the_mgmt_port_the_host_named() {
        val store = KnownHostStore(context)
        val host = store.trust("192.168.1.9", 9777, "Desk", "ab".repeat(32), paired = true)
        val session = SessionFactory.afterDial(7L, host, Settings(), preset = null, store, mgmtPort = 47991)
        assertEquals(47991, store.byId(host.id)?.mgmtPort)
        assertEquals(host.id, session.hostId)
        assertEquals(7L, session.handle)
    }

    @Test
    fun an_unsaved_host_gets_no_clipboard_and_no_record() {
        val store = KnownHostStore(context)
        val session = SessionFactory.afterDial(7L, null, Settings(), preset = null, store, mgmtPort = 47991)
        assertFalse(session.clipboardSync)
        assertNull(session.hostId)
        assertEquals(0, store.all().size)
    }
}
