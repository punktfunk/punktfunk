package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.DisplayTargets.DisplayTarget
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** `GET /display/targets` and the pick's body, as the host spells them (`screen-pick.md` §4). */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class DisplayTargetsTest {
    private val res get() = ApplicationProvider.getApplicationContext<Context>().resources

    private val plan = """{ "locked": false, "host_default": "own", "yours": {"monitor": "HDMI-A-1"},
      "targets": [
        { "id": "own", "label": "Own screen" },
        { "id": "HDMI-A-1", "label": "LG TV", "connector": "HDMI-A-1", "mode": "3840x2160@60",
          "primary": true, "enabled": true, "in_use_by": null },
        { "id": "DP-2", "label": "DELL U2723QE", "connector": "DP-2", "mode": "3840x2160@60",
          "primary": false, "enabled": false, "in_use_by": null } ] }"""

    @Test
    fun thePlansBodyParses() {
        val t = DisplayTargets.parse(plan)!!
        assertEquals(false, t.locked)
        assertEquals(DisplayTarget.Own, t.hostDefault)
        val tv = DisplayTarget.Monitor("HDMI-A-1")
        assertEquals(tv, t.yours)
        assertEquals(tv, t.current())
        val own = t.targets[0]
        assertEquals(DisplayTarget.Own, own.target())
        assertTrue("an own row without the field is pickable", own.enabled)
        assertEquals("Own screen", own.name(res))
        assertNull(own.reason(res))
        assertEquals("LG TV", t.targets[1].name(res))
        assertEquals("HDMI-A-1 · 3840x2160@60", t.targets[1].detail())
        assertEquals("Off", t.targets[2].reason(res))
        assertEquals("Streams LG TV", t.summary(res))
    }

    @Test
    fun yoursAbsentOrNullFollowsTheHost() {
        for (body in listOf(
            """{"host_default": {"monitor": "DP-1"}, "targets": []}""",
            """{"host_default": {"monitor": "DP-1"}, "yours": null}""",
        )) {
            val t = DisplayTargets.parse(body)!!
            assertNull(t.yours)
            assertEquals(DisplayTarget.Monitor("DP-1"), t.current())
            assertEquals("an unlisted head names its connector", "Streams DP-1", t.summary(res))
        }
        assertEquals("Own screen", DisplayTargets.parse("{}")!!.summary(res))
        assertNull(DisplayTargets.parse("[]"))
    }

    @Test
    fun aTargetEncodesAsTheHostSpellsIt() {
        assertEquals("own", DisplayTarget.Own.json())
        val tv = DisplayTarget.Monitor("HDMI-A-1").json()
        assertEquals("""{"monitor":"HDMI-A-1"}""", tv.toString())
        assertEquals("""{"screen":"own"}""", JSONObject().put("screen", DisplayTarget.Own.json()).toString())
        assertEquals(DisplayTarget.Monitor("HDMI-A-1"), DisplayTarget.of(tv))
        assertEquals(DisplayTarget.Own, DisplayTarget.of("own"))
        assertNull(DisplayTarget.of("HDMI-A-1"))
    }

    /** The console's `HostRow.screens` is the host's shape, so it reads back as what came in. */
    @Test
    fun theConsoleRowReadsBackAsTheHostsBody() {
        val t = DisplayTargets.parse(plan)!!
        assertEquals(t, DisplayTargets.parse(t.json().toString()))
    }

    @Test
    fun aTakenHeadSaysWhoHasIt() {
        val row = DisplayTargets.Target(
            id = "HDMI-A-1", label = "HDMI-A-1", connector = "HDMI-A-1", mode = "1920x1080@60",
            inUseBy = "Ben's Apple TV",
        )
        assertEquals("Streamed by Ben's Apple TV", row.reason(res))
        assertEquals("the label already names the connector", "1920x1080@60", row.detail())
    }
}
