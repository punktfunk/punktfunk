package io.unom.punktfunk

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/render-scale-vectors.json` against [RenderScale] — the same cases core and the
 * Swift twin run, so every client asks the host for the same `Mode`. Run:
 * `./gradlew :app:testDebugUnitTest`.
 */
class RenderScaleTest {
    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val file = File("../../shared/render-scale-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    /** JSON has no NaN; the file writes it as null. */
    private fun JSONObject.num(key: String) = if (isNull(key)) Double.NaN else getDouble(key)

    @Test
    fun everySharedVectorAgrees() {
        val dims = vectors.getJSONArray("max_dimension")
        for (i in 0 until dims.length()) {
            val row = dims.getJSONObject(i)
            val codec = row.getString("codec")
            assertEquals(codec, row.getInt("max"), RenderScale.maxDimension(codec))
        }
        val sanitize = vectors.getJSONArray("sanitize")
        for (i in 0 until sanitize.length()) {
            val row = sanitize.getJSONObject(i)
            assertEquals("$row", row.num("want"), RenderScale.sanitize(row.num("raw")), 0.0)
        }
        val percents = vectors.getJSONArray("percent")
        for (i in 0 until percents.length()) {
            val row = percents.getJSONObject(i)
            val want = row.num("want")
            assertEquals("$row", want, RenderScale.fromPercent(row.getInt("typed")), 0.0)
            assertEquals("$row", row.getInt("reads"), RenderScale.percent(want))
        }
        val presets = vectors.getJSONArray("preset")
        for (i in 0 until presets.length()) {
            val row = presets.getJSONObject(i)
            val index = if (row.isNull("index")) null else row.getInt("index")
            assertEquals("$row", index, RenderScale.presetIndex(row.num("scale")))
        }
        val cases = vectors.getJSONArray("apply")
        assertTrue("the vector file is the contract; keep it rich", cases.length() >= 12)
        for (i in 0 until cases.length()) {
            val case = cases.getJSONObject(i)
            val base = case.getJSONArray("base")
            val want = case.getJSONArray("want")
            val got = RenderScale.apply(
                base.getInt(0),
                base.getInt(1),
                case.num("scale"),
                RenderScale.maxDimension(case.getString("codec")),
            )
            assertEquals(case.getString("name"), want.getInt(0) to want.getInt(1), got)
        }
        val views = vectors.getJSONArray("view")
        assertTrue("the view rows are the contract too", views.length() >= 8)
        for (i in 0 until views.length()) {
            val row = views.getJSONObject(i)
            val base = row.getJSONArray("base")
            val want = row.optJSONArray("want")?.let { it.getInt(0) to it.getInt(1) }
            val got = RenderScale.view(base.getInt(0), base.getInt(1), row.num("scale"), row.getInt("max"))
            assertEquals("$row", want, got)
        }
    }

    /**
     * Above 1x the connect names the chosen size as its view beside the scaled mode; 1x and below
     * name none. An override that brings the mode down to the view drops it.
     */
    @Test
    fun theConnectNamesTheViewAbove1x() {
        val base = Triple(1366, 768, 60)
        val panel = Triple(1920, 1080, 60)
        fun ask(scale: Double, lines: Int? = null): Pair<Pair<Int, Int>, Pair<Int, Int>?> {
            val o = lines?.let { io.unom.punktfunk.kit.library.GameOverrides(lines = it) }
            val (mode, view) = Settings(renderScale = scale).streamAsk(base, o) { panel }
            return (mode.first to mode.second) to view
        }
        val view = 1366 to 768
        assertEquals((2048 to 1152) to view, ask(1.5))
        assertEquals((2732 to 1536) to view, ask(2.0))
        assertEquals((1366 to 768) to null, ask(1.0))
        assertEquals((682 to 384) to null, ask(0.5))
        assertEquals((1920 to 1080) to view, ask(2.0, lines = 1080))
        assertEquals((1366 to 768) to null, ask(2.0, lines = 768))
        assertEquals((1280 to 720) to null, ask(2.0, lines = 720))
    }
}
