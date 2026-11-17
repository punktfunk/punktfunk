package io.unom.punktfunk

import io.unom.punktfunk.components.resolveOsIcon
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * Pure JVM test of the host card's OS marks (`components/OsIcons.kt`). Run:
 * `./gradlew :app:testDebugUnitTest`.
 *
 * The aspect assertions are the point: a [androidx.compose.ui.graphics.vector.VectorPainter] maps
 * the viewport onto the vector's default size with independent x and y scales, so a mark whose
 * default size does not carry its viewport's ratio renders STRETCHED — silently, with no crash and
 * no warning.
 */
class OsIconsTest {
    /** `ImageVector.name` is "OsIcon.<token>" — the only handle on which mark got resolved. */
    private fun markOf(chain: String) = resolveOsIcon(chain)?.name?.removePrefix("OsIcon.")

    @Test
    fun defaultSizeCarriesTheViewportAspect() {
        for (chain in listOf("windows", "linux", "opensuse", "steam", "apple", "bazzite")) {
            val v = resolveOsIcon(chain)
            assertNotNull("no mark for $chain", v)
            v!!
            assertEquals(
                "$chain default size must keep the viewport ratio",
                v.viewportWidth / v.viewportHeight,
                v.defaultWidth.value / v.defaultHeight.value,
                0.001f,
            )
            assertEquals(
                "$chain longest edge must be the 24dp box",
                24f,
                maxOf(v.defaultWidth.value, v.defaultHeight.value),
                0.001f,
            )
        }
    }

    @Test
    fun tallMarkIsNarrowerThanItsBox() {
        // Tux is 448x512, so a correct build is 21x24dp — 24x24 would be the stretched bug.
        val tux = resolveOsIcon("linux")!!
        assertEquals(21f, tux.defaultWidth.value, 0.001f)
        assertEquals(24f, tux.defaultHeight.value, 0.001f)
    }

    @Test
    fun distroLeavesResolveToTheirOwnMark() {
        // The whole reason these four ship art: without it they'd draw their family's mark.
        assertEquals("bazzite", markOf("linux/fedora/bazzite"))
        assertEquals("cachyos", markOf("linux/arch/cachyos"))
        assertEquals("nobara", markOf("linux/rhel/nobara"))
        assertEquals("omarchy", markOf("linux/arch/omarchy"))
    }

    @Test
    fun unknownDistroStillDegradesThroughItsFamily() {
        assertEquals("fedora", markOf("linux/fedora/somethingnew"))
        assertEquals("linux", markOf("linux/frontier/chimera"))
        assertEquals("steam", markOf("linux/arch/steamos")) // brand alias
        assertEquals("apple", markOf("macos"))
    }

    @Test
    fun noChainMeansNoMark() {
        assertNull(resolveOsIcon(""))
        assertNull(resolveOsIcon("!!!"))
    }
}
