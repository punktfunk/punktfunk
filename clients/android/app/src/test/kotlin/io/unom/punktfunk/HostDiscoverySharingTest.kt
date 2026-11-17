package io.unom.punktfunk

import android.content.Context
import android.os.Looper
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.HostDiscovery
import java.time.Duration
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertSame
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/**
 * The connect screen and the Skia console are both on this browse, and each mDNS daemon binds
 * :5353 and joins the multicast groups for itself — two of them split the same answers. So the
 * instance is shared and the subscriptions are counted: what this pins is that the arithmetic
 * holds, since a browse that quietly stops (or quietly doubles) looks like a network problem.
 *
 * The native library is absent under Robolectric, so the browse never reaches "running"; every
 * assertion here is about the bookkeeping that decides whether it is asked to.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric has no SDK 37 image yet; the app targets 37
class HostDiscoverySharingTest {
    private val context: Context get() = ApplicationProvider.getApplicationContext()

    /**
     * Each subscriber captures its own sink, which is what makes it a distinct object: Kotlin
     * compiles a lambda that captures nothing to a singleton, so two plain `{ }` subscribers would
     * be one, and the counting below would pass against code that cannot count.
     */
    private fun subscriber(seen: MutableList<Int> = mutableListOf()): (List<DiscoveredHost>) -> Unit =
        { hosts -> seen += hosts.size }

    @Test
    fun every_caller_gets_the_same_browse() {
        assertSame(HostDiscovery.shared(context), HostDiscovery.shared(context))
    }

    @Test
    fun subscriptions_are_counted_and_a_repeat_is_not_a_second_one() {
        val discovery = HostDiscovery.shared(context)
        val screen = subscriber()
        val console = subscriber()
        try {
            discovery.addListener(screen)
            discovery.addListener(screen) // the console re-attaching over a live screen
            assertEquals(1, discovery.listenerCount)

            // Both UIs up: the one that leaves must not take the browse with it.
            discovery.addListener(console)
            assertEquals(2, discovery.listenerCount)
            discovery.removeListener(screen)
            assertEquals(1, discovery.listenerCount)

            discovery.removeListener(console)
            assertEquals(0, discovery.listenerCount)
            discovery.removeListener(console) // a dispose after a pause already dropped it
            assertEquals(0, discovery.listenerCount)
        } finally {
            discovery.removeListener(screen)
            discovery.removeListener(console)
        }
    }

    /**
     * A grant that lands while nobody is subscribed (mid-stream, from system settings) rebuilds
     * nothing: a browse with no subscriber would run beside the session, and nothing would ever
     * stop it. Without the native library a start that IS attempted fails and schedules its
     * retry, which is how "attempted" shows up here.
     */
    @Test
    fun a_restart_with_no_subscriber_builds_no_browse() {
        val discovery = HostDiscovery.shared(context)
        val looper = shadowOf(Looper.getMainLooper())
        looper.idleFor(Duration.ofSeconds(5)) // let an earlier test's browse quiesce
        discovery.restart()
        assertEquals(Duration.ZERO, looper.nextScheduledTaskTime)
        val screen = subscriber()
        discovery.onAppStart()
        try {
            discovery.addListener(screen)
            assertNotEquals(Duration.ZERO, looper.nextScheduledTaskTime)
        } finally {
            discovery.onAppStop()
            discovery.removeListener(screen)
        }
    }

    /** Home with a subscriber still attached ends the browse; coming back starts it again. */
    @Test
    fun the_browse_sleeps_while_the_app_is_away() {
        val discovery = HostDiscovery.shared(context)
        val looper = shadowOf(Looper.getMainLooper())
        looper.idleFor(Duration.ofSeconds(5))
        val screen = subscriber()
        discovery.onAppStart()
        try {
            discovery.addListener(screen)
            assertNotEquals(Duration.ZERO, looper.nextScheduledTaskTime) // a start was attempted
            discovery.onAppStop()
            assertEquals(Duration.ZERO, looper.nextScheduledTaskTime)
            assertEquals(1, discovery.listenerCount)
            discovery.onAppStart()
            assertNotEquals(Duration.ZERO, looper.nextScheduledTaskTime)
        } finally {
            discovery.onAppStop()
            discovery.removeListener(screen)
        }
    }
}
