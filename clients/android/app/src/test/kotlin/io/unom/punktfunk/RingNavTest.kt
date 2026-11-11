package io.unom.punktfunk

import androidx.activity.ComponentActivity
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.hasScrollAction
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.RingNav
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** A TV remote has only the D-pad and OK, so every stop on the dial and every sheet row must be
 *  reachable from them. `sdk = [36]`: Robolectric's newest android-all. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class RingNavTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    @Test
    fun upFromTwelveOrDownFromSixStepsIntoTheCentre() {
        val ctx = compose.activity
        val state = RingState()
        val cfg = OverlayConfig(List(6) { null })
        val press = { n: RingNav ->
            navigate(n, state, emptyList(), cfg, previewActionsIn(ctx), ConsoleHaptics(null), ctx) { _, _ -> }
        }
        press(RingNav.Up)
        assertEquals(0, state.highlight)
        press(RingNav.Up)
        assertEquals(6, state.highlight)
        press(RingNav.Down)
        assertEquals(3, state.highlight)
        press(RingNav.Down)
        assertEquals(6, state.highlight)
        // Elsewhere on the ring, Up and Down still jump to 12 and 6.
        press(RingNav.Right)
        press(RingNav.Right)
        press(RingNav.Up)
        assertEquals(0, state.highlight)
        press(RingNav.Left)
        press(RingNav.Down)
        assertEquals(3, state.highlight)
    }

    /** The cursor's row and the one it came from stay whole on the panel, both ways down the
     *  list: the scroll moves a row at a time, not a page. */
    @Test
    fun theSheetScrollsOneRowAtATimeWithTheCursor() {
        val state = RingState()
        val rows = List(20) { i ->
            SheetRowSpec(header = if (i % 5 == 0) "Group ${i / 5}" else null, label = "Row $i") {}
        }
        compose.setContent {
            Box(Modifier.size(360.dp, 300.dp)) {
                RingSheet(state, rows, ConsoleHaptics(null), Modifier.align(Alignment.BottomCenter))
            }
        }
        fun assertWhole(i: Int) {
            val panel = compose.onNode(hasScrollAction()).fetchSemanticsNode()
            val row = compose.onNodeWithText("Row $i").fetchSemanticsNode()
            val top = row.positionInRoot.y
            val bottom = top + row.size.height
            assertTrue("row $i top $top above the panel", top >= panel.positionInRoot.y)
            assertTrue("row $i bottom $bottom below the panel", bottom <= panel.positionInRoot.y + panel.size.height)
        }
        fun moveTo(i: Int) {
            compose.runOnIdle { state.sheetCursor = i }
            compose.waitForIdle()
        }
        for (i in 1..rows.lastIndex) {
            moveTo(i)
            assertWhole(i)
            assertWhole(i - 1)
        }
        for (i in rows.lastIndex - 1 downTo 0) {
            moveTo(i)
            assertWhole(i)
            assertWhole(i + 1)
        }
    }
}
