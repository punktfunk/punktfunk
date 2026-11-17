package io.unom.punktfunk

import android.os.Looper
import io.unom.punktfunk.console.MainConfined
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/** The console's state is reached on the main thread only; any other thread hands work over. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric has no SDK 37 image yet; the app targets 37
class MainConfinedTest {
    @Test
    fun another_thread_hands_work_over_and_cannot_touch_the_state() {
        val confined = MainConfined(mutableListOf<String>())
        confined.now { add("main") }
        var refused: Throwable? = null
        val worker = Thread {
            refused = runCatching { confined.now { add("worker") } }.exceptionOrNull()
            confined.launch { add(if (Looper.myLooper() == Looper.getMainLooper()) "handed over" else "off main") }
        }
        worker.start()
        worker.join()
        shadowOf(Looper.getMainLooper()).idle()
        assertTrue(refused is IllegalStateException)
        assertEquals(listOf("main", "handed over"), confined.now { toList() })
    }
}
