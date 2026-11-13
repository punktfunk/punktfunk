package io.unom.punktfunk

import io.unom.punktfunk.console.ConsoleJson
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.nameOrder
import io.unom.punktfunk.kit.security.KnownHost
import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/host-row-vectors.json` against [ConsoleJson.hostRows] — the rows the Swift and
 * desktop producers send too, so a player moving between devices finds one carousel.
 */
class HostRowVectorTest {
    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val file = File("../../shared/host-row-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    private fun JSONArray.strings() = List(length()) { getString(it) }
    private fun JSONObject.optIntOrNull(k: String) = if (isNull(k)) null else getInt(k)
    private fun JSONObject.optLongOrNull(k: String) = if (isNull(k)) null else getLong(k)

    @Test
    fun everySharedVectorAgrees() {
        val cases = vectors.getJSONArray("cases")
        for (i in 0 until cases.length()) {
            val case = cases.getJSONObject(i)
            val name = case.getString("name")
            val saved = case.getJSONArray("saved").let { a ->
                List(a.length()) { a.getJSONObject(it) }.map {
                    KnownHost(
                        address = it.getString("addr"),
                        port = it.getInt("port"),
                        name = it.getString("name"),
                        fpHex = it.getString("fp"),
                        paired = it.getString("fp").isNotEmpty(),
                        mac = it.getJSONArray("mac").strings(),
                        os = it.getString("os"),
                        mgmtPort = it.optIntOrNull("mgmt_port"),
                        id = it.getString("id"),
                        pinnedPresetIds = it.getJSONArray("pins").strings(),
                        addedAt = it.optLongOrNull("added"),
                        lastUsed = it.optLongOrNull("last_used"),
                    )
                }
            }
            val discovered = case.getJSONArray("discovered").let { a ->
                List(a.length()) { a.getJSONObject(it) }.map {
                    DiscoveredHost(
                        key = it.getString("name"),
                        name = it.getString("name"),
                        host = it.getString("addr"),
                        port = it.getInt("port"),
                        fingerprint = it.getString("fp"),
                        os = it.getString("os"),
                        mgmtPort = it.optIntOrNull("mgmt_port"),
                    )
                }
            }
            val presets = case.getJSONArray("presets").strings().map { StreamPreset(id = it, name = it) }
            // The store hands its records over by name; the added stamps restore store order.
            val rows = JSONArray(
                ConsoleJson.hostRows(
                    saved.sortedWith(nameOrder { it.name }),
                    discovered,
                    case.getJSONArray("online").strings().toSet(),
                    presets,
                ),
            )
            val want = case.getJSONArray("rows")
            assertEquals("$name row count", want.length(), rows.length())
            for (r in 0 until want.length()) {
                val w = want.getJSONObject(r)
                val got = rows.getJSONObject(r)
                val at = "$name row $r"
                assertEquals("$at key", w.getString("key"), got.getString("key"))
                for (k in listOf("saved", "online", "can_wake")) {
                    assertEquals("$at $k", w.getBoolean(k), got.getBoolean(k))
                }
                assertEquals("$at mgmt_port", w.getInt("mgmt_port"), got.getInt("mgmt_port"))
                assertEquals("$at os", w.getString("os"), got.getString("os"))
                assertEquals("$at last_used", w.optLongOrNull("last_used"), got.optLongOrNull("last_used"))
                val pin = got.optJSONObject("pin")?.getString("id")
                assertEquals("$at pin", if (w.isNull("pin")) null else w.getString("pin"), pin)
            }
        }
    }

    /** A preset chip carries the host screen it pins, for the console's card under that preset. */
    @Test
    fun aPresetChipCarriesItsHostScreen() {
        val couch = StreamPreset("couch", "Couch", overrides = SettingsOverlay(hostScreen = "second"))
        val desk = KnownHost(
            "10.0.0.2", 9777, "Desk", "ab12", paired = true,
            presetId = "couch", pinnedPresetIds = listOf("couch"),
        )
        val rows = JSONArray(ConsoleJson.hostRows(listOf(desk), emptyList(), emptySet(), listOf(couch)))
        assertEquals("second", rows.getJSONObject(0).getJSONObject("bound_preset").getString("host_screen"))
        assertEquals("second", rows.getJSONObject(1).getJSONObject("pin").getString("host_screen"))
    }
}
