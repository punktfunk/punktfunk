package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.DisplayTargets.DisplayTarget
import io.unom.punktfunk.DisplayTargets.Pick
import io.unom.punktfunk.kit.R
import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** `GET /display/targets`, the Screen section drawn from it, and the pick's body, as the host spells them. */
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
        assertEquals("Streams LG TV", t.cardLine(res))
        assertTrue("a host without the grant lets every device choose", t.mayChoose)
        assertNull(t.role)
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
            assertEquals("an unlisted head names its connector", "Streams DP-1", t.cardLine(res))
        }
        assertEquals(DisplayTargets.Targets(), DisplayTargets.parse("{}"))
        assertNull(DisplayTargets.parse("{}")!!.cardLine(res))
        assertNull(DisplayTargets.parse("[]"))
    }

    @Test
    fun aTargetEncodesAsTheHostSpellsIt() {
        assertEquals("own", DisplayTarget.Own.json())
        val tv = DisplayTarget.Monitor("HDMI-A-1").json()
        assertEquals("""{"monitor":"HDMI-A-1"}""", tv.toString())
        assertEquals(DisplayTarget.Monitor("HDMI-A-1"), DisplayTarget.of(tv))
        assertEquals(DisplayTarget.Own, DisplayTarget.of("own"))
        assertNull(DisplayTarget.of("HDMI-A-1"))
        assertEquals("""{"screen":"own"}""", Pick(DisplayTarget.Own).json().toString())
        val second = Pick(DisplayTarget.Own, "extend").json()
        assertEquals(2, second.length())
        assertEquals("own", second.getString("screen"))
        assertEquals("extend", second.getString("topology"))
        assertEquals(Pick(DisplayTarget.Own, "extend"), Pick.of(second))
    }

    /** The console's `HostRow.screens` is the host's shape, so it reads back as what came in. */
    @Test
    fun theConsoleRowReadsBackAsTheHostsBody() {
        val t = DisplayTargets.parse(plan)!!
        assertEquals(t, DisplayTargets.parse(t.json().toString()))
        val decided = DisplayTargets.parse(
            """{"may_choose": false, "host_default": "own", "yours": "own", "targets": [],
              "role": {"yours": "second", "host_default": "main", "offered": ["only", "second"]}}""",
        )!!
        assertEquals(false, decided.mayChoose)
        assertEquals(DisplayTargets.Role.SECOND, decided.role?.current())
        assertEquals(decided, DisplayTargets.parse(decided.json().toString()))
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

    /** A vector text slot as this build words it: null, the host's string, or a message by key. */
    private fun text(v: Any?): String? = when (v) {
        null, JSONObject.NULL -> null
        is String -> v
        is JSONObject -> {
            val id = R.string::class.java.getField(v.getString("key")).getInt(null)
            // The placeholders in the order the English text shows them.
            val args = v.optJSONObject("args")?.let { a -> a.keys().asSequence().map(a::getString).toList() }.orEmpty()
            if (args.isEmpty()) res.getString(id) else res.getString(id, *args.toTypedArray())
        }
        else -> error("not a text slot: $v")
    }

    /** `clients/shared/screen-rows-vectors.json`: each answer's rows, note and card line. */
    @Test
    fun sharedVectors() {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val file = File("../../shared/screen-rows-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        val cases = JSONObject(file.readText()).getJSONArray("cases")
        for (i in 0 until cases.length()) {
            val case = cases.getJSONObject(i)
            val name = case.getString("name")
            val t = DisplayTargets.parse(case.getJSONObject("targets").toString())!!
            val got = t.rows(res).map { r ->
                listOf(
                    r.kind.name.lowercase(), r.id, r.label, r.hint, r.detail, r.header, r.checked,
                    r.pickable, r.reason, r.pick?.json()?.let(Pick::of),
                )
            }
            val rows = case.getJSONArray("rows")
            val want = (0 until rows.length()).map { j ->
                val w = rows.getJSONObject(j)
                listOf(
                    w.getString("kind"), w.getString("id"), text(w.get("label")), text(w.get("hint")),
                    w.getString("detail"), text(w.get("header")), w.getBoolean("checked"),
                    w.getBoolean("pickable"), text(w.get("reason")), w.optJSONObject("put")?.let(Pick::of),
                )
            }
            assertEquals(name, want, got)
            assertEquals(name, text(case.get("note")), t.note(res))
            assertEquals(name, text(case.get("card")), t.cardLine(res))
        }
    }
}
