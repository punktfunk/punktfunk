package io.unom.punktfunk.kit

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/connect-request-vectors.json` against [ConnectRequest.toJson]. The Rust side
 * (`session/connect.rs`, `request_vectors`) replays the same file, so a key renamed here or there
 * fails a test instead of reading as its default.
 */
class ConnectRequestVectorTest {
    private val cases by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/kit).
        val file = File("../../shared/connect-request-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText()).getJSONArray("cases")
    }

    private fun JSONObject.text(key: String): String? = if (isNull(key)) null else getString(key)

    private fun JSONObject.request() = ConnectRequest(
        host = getString("host"),
        port = getInt("port"),
        width = getInt("width"),
        height = getInt("height"),
        refreshHz = getInt("refreshHz"),
        certPem = getString("certPem"),
        keyPem = getString("keyPem"),
        pinHex = getString("pinHex"),
        bitrateKbps = getInt("bitrateKbps"),
        compositorPref = getInt("compositorPref"),
        gamepadPref = getInt("gamepadPref"),
        hdrEnabled = getBoolean("hdrEnabled"),
        tenBitSdr = getBoolean("tenBitSdr"),
        multiSliceOk = getBoolean("multiSliceOk"),
        framePartsOk = getBoolean("framePartsOk"),
        audioChannels = getInt("audioChannels"),
        audioRateHz = getInt("audioRateHz"),
        audioBits = getInt("audioBits"),
        videoCodecs = getInt("videoCodecs"),
        preferredCodec = getInt("preferredCodec"),
        timeoutMs = getInt("timeoutMs"),
        launch = text("launch"),
        deviceName = text("deviceName"),
        padAudioOk = getBoolean("padAudioOk"),
        keepHostAudio = getBoolean("keepHostAudio"),
        clientCursor = getBoolean("clientCursor"),
        videoFit = getString("videoFit"),
        dialer = getString("dialer"),
        presetId = text("presetId"),
        presetName = text("presetName"),
        deliveryFlags = getInt("deliveryFlags"),
        profile = text("profile"),
        pyrowaveBppX100 = getInt("pyrowaveBppX100"),
        studioRange = getBoolean("studioRange"),
        viewWidth = getInt("viewWidth"),
        viewHeight = getInt("viewHeight"),
        hostScreen = getString("hostScreen"),
    )

    /** One JSON value, whatever the key order or number spelling. */
    private fun same(a: Any?, b: Any?): Boolean = when {
        a is JSONObject && b is JSONObject -> a.length() == b.length() &&
            a.keys().asSequence().all { b.has(it) && same(a.get(it), b.get(it)) }
        a is Number && b is Number -> a.toDouble() == b.toDouble()
        else -> a == b
    }

    @Test
    fun toJsonWritesEveryVector() {
        assertTrue("the vector file is the contract; keep two cases", cases.length() >= 2)
        for (i in 0 until cases.length()) {
            val case = cases.getJSONObject(i)
            val want = case.getJSONObject("json")
            val got = JSONObject(case.getJSONObject("input").request().toJson())
            assertTrue("${case.getString("name")}: wrote $got, not $want", same(want, got))
        }
    }
}
