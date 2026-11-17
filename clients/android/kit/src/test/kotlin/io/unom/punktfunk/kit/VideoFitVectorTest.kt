package io.unom.punktfunk.kit

import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/video-fit-vectors.json` against [VideoFit.place] — the same cases the Rust and
 * Swift placements run, so a letterbox on this client cannot land a pixel off theirs.
 */
class VideoFitVectorTest {
    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/kit).
        val file = File("../../shared/video-fit-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    private fun JSONArray.ints() = IntArray(length()) { getInt(it) }

    @Test
    fun everySharedVectorAgrees() {
        val cases = vectors.getJSONArray("cases")
        assertTrue("the vector file is the contract; keep it rich", cases.length() >= 12)
        for (i in 0 until cases.length()) {
            val case = cases.getJSONObject(i)
            val name = case.getString("name")
            val view = case.getJSONArray("view").ints()
            val frame = case.getJSONArray("frame").ints()
            val p = VideoFit.place(
                VideoFit.fromName(case.getString("fit")), view[0], view[1], frame[0], frame[1],
            )
            val want = case.getJSONObject("expect")
            val dst = want.getJSONArray("dst").ints()
            assertEquals("$name dst", dst.toList(), listOf(p.dstX, p.dstY, p.dstW, p.dstH))
            val src = want.getJSONArray("src")
            listOf(p.srcX, p.srcY, p.srcW, p.srcH).forEachIndexed { k, got ->
                assertEquals("$name src[$k]", src.getDouble(k), got, 1e-4)
            }
            val kernel = want.getJSONArray("kernel")
            assertEquals("$name kernel x", kernel.getString(0), p.kernelX.wire)
            assertEquals("$name kernel y", kernel.getString(1), p.kernelY.wire)
            val maps = want.optJSONArray("to_frame") ?: JSONArray()
            for (m in 0 until maps.length()) {
                val row = maps.getJSONArray(m)
                assertEquals("$name to_frame x", row.getDouble(2), p.frameX(row.getDouble(0)), 1e-4)
                assertEquals("$name to_frame y", row.getDouble(3), p.frameY(row.getDouble(1)), 1e-4)
            }
        }
    }

    @Test
    fun unknownNamesReadAsFit() {
        assertEquals(VideoFit.FIT, VideoFit.fromName("zoom"))
        assertEquals(VideoFit.FIT, VideoFit.fromName(null))
        VideoFit.entries.forEach { assertEquals(it, VideoFit.fromName(it.wire)) }
    }
}
