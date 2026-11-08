package io.unom.punktfunk.kit

import io.unom.punktfunk.kit.Shortcuts.Out
import io.unom.punktfunk.kit.Shortcuts.Phase
import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/shortcut-vectors.json` against [Shortcuts], every row of its four sections, as
 * the Rust `shortcuts/tests.rs` replays them. A port that disagrees reports it; no row is edited
 * to make this pass.
 */
class ShortcutVectorsTest {
    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/kit).
        val file = File("../../shared/shortcut-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    private fun section(name: String): List<JSONObject> =
        vectors.getJSONObject(name).getJSONArray("cases").let { a -> (0 until a.length()).map(a::getJSONObject) }

    private fun blob(bindings: JSONArray) = JSONObject().put("v", 1).put("bindings", bindings).toString()

    /** JSON equality with object keys in any order. */
    private fun same(a: Any?, b: Any?): Boolean = when {
        a is JSONArray && b is JSONArray -> a.length() == b.length() && (0 until a.length()).all { same(a.get(it), b.get(it)) }
        a is JSONObject && b is JSONObject -> a.keys().asSequence().toSet().let { keys ->
            keys == b.keys().asSequence().toSet() && keys.all { same(a.get(it), b.get(it)) }
        }
        else -> a == b
    }

    private fun client(name: String) = Shortcuts.Client.valueOf(name.uppercase())

    @Test
    fun parse() {
        val cases = section("parse")
        assertTrue(cases.size >= 14)
        for (c in cases) {
            val name = c.getString("name")
            val raw = c.getString("blob")
            val got = JSONObject(Shortcuts.parse(raw).toJson())
            assertTrue("$name: $got", same(got, c.getJSONObject("expect")))
            assertEquals("$name: defaults", c.getBoolean("defaults"), Shortcuts.read(raw) == null)
        }
    }

    @Test
    fun defaults() {
        val cases = section("defaults")
        assertTrue(cases.size >= 13)
        for (c in cases) {
            val cl = client(c.getString("client"))
            val gameMode = c.getBoolean("game_mode")
            val legacy = c.getString("guide_gesture")
            val d = Shortcuts.defaults(cl, gameMode, legacy)
            val got = JSONObject(d.toJson()).getJSONArray("bindings")
            assertTrue("$c: $got", same(got, c.getJSONArray("expect")))
            assertEquals(d, Shortcuts.resolve("", cl, gameMode, legacy))
            assertEquals("$cl round-trips", d, Shortcuts.parse(d.toJson()))
        }
    }

    @Test
    fun validate() {
        val cases = section("validate")
        assertTrue(cases.size >= 13)
        for (c in cases) {
            val name = c.getString("name")
            val have = Shortcuts.parse(blob(c.getJSONArray("bindings")))
            assertEquals(name, c.getJSONArray("bindings").length(), have.bindings.size)
            val candidate = Shortcuts.parse(blob(JSONArray().put(c.getJSONObject("candidate")))).bindings.single()
            val want: Shortcuts.Conflict? = when (val e = c.getString("expect")) {
                "ok" -> null
                "identical" -> Shortcuts.Conflict.Identical(c.getInt("index"))
                "press_subset" -> Shortcuts.Conflict.PressSubset(c.getInt("index"))
                "single_press" -> Shortcuts.Conflict.SinglePress
                "no_modifier" -> Shortcuts.Conflict.NoModifier
                "no_key" -> Shortcuts.Conflict.NoKey
                else -> error("$name: an expect the vectors use: $e")
            }
            assertEquals(name, want, have.validate(candidate))
        }
    }

    /** Events and ticks merge by t, events first and in order, then the tick; flush runs last. */
    @Test
    fun engine() {
        val cases = section("engine")
        assertTrue(cases.size >= 28)
        for (c in cases) {
            val name = c.getString("name")
            val pad = when (val d = c.getString("device")) {
                "pad" -> true
                "key" -> false
                else -> error("$name: a device: $d")
            }
            fun id(n: String): Int =
                (if (pad) Shortcuts.buttonBit(n) else keyVk(n)) ?: error("$name: a name: $n")
            val bindings = Shortcuts.parse(blob(c.getJSONArray("bindings"))).bindings
            assertEquals(name, c.getJSONArray("bindings").length(), bindings.size)
            val engine = Shortcuts.Engine(bindings, if (pad) Shortcuts.Device.PAD else Shortcuts.Device.KEYBOARD)

            data class Step(val t: Long, val order: Int, val id: Int = 0, val down: Boolean = false)
            val events = c.getJSONArray("events")
            val ticks = c.getJSONArray("ticks")
            val timeline = (0 until events.length()).map { events.getJSONObject(it) }
                .map { Step(it.getLong("t"), 0, id(it.getString("bit")), it.getBoolean("down")) } +
                (0 until ticks.length()).map { Step(ticks.getLong(it), 1) } +
                listOfNotNull(c.optLong("flush", -1).takeIf { it >= 0 }?.let { Step(it, 2) })
            val got = timeline.sortedWith(compareBy({ it.t }, { it.order })).flatMap { s ->
                when (s.order) {
                    0 -> engine.step(s.id, s.down, s.t)
                    1 -> engine.tick(s.t)
                    else -> engine.flush()
                }.map { s.t to it }
            }

            val expect = c.getJSONArray("expect")
            val want = (0 until expect.length()).map { expect.getJSONObject(it) }.map { e ->
                val o = e.getJSONObject("out")
                val out = when {
                    o.has("forward") -> o.getJSONArray("forward").let { Out.Forward(id(it.getString(0)), it.getBoolean(1)) }
                    o.has("fire") -> o.getJSONArray("fire").let {
                        Out.Fire(Shortcuts.Action.parse(it.getString(0)), Phase.valueOf(it.getString(1).uppercase()))
                    }
                    else -> o.getJSONArray("lift").let { a -> Out.Lift((0 until a.length()).map { id(a.getString(it)) }) }
                }
                e.getLong("t") to out
            }
            assertEquals(name, want, got)
        }
    }

    /** The editor checks a changed row against the others; every default row passes its own check. */
    @Test
    fun everyDefaultRowCanBeEditedInPlace() {
        for (c in Shortcuts.Client.entries) {
            val d = Shortcuts.defaults(c, gameMode = true, legacyGuideGesture = "on")
            d.bindings.forEachIndexed { i, b ->
                assertEquals("$c row $i: $b", null, Shortcuts(d.bindings.filterIndexed { j, _ -> j != i }).validate(b))
            }
        }
    }

    @Test
    fun everyButtonNameRoundTrips() {
        for ((name, bit) in Shortcuts.BUTTONS) {
            assertEquals(bit, Shortcuts.buttonBit(name))
            assertEquals(name, Shortcuts.buttonName(bit))
        }
        assertEquals(Shortcuts.buttonBit("select"), Shortcuts.buttonBit("BACK"))
    }

    /** The banner names the live trigger, whatever the player moved it to. */
    @Test
    fun aHintNamesTheLiveTrigger() {
        val end = Shortcuts.Action.Slot(SlotId.EndStream)
        val android = Shortcuts.defaults(Shortcuts.Client.ANDROID, false, "auto")
        assertEquals(say(R.string.controls_hold, "L1 + R1 + Start + Select"), android.hint(end, Shortcuts.Device.PAD))
        assertEquals(Say.Text("Ctrl+Alt+Shift+O"), android.hint(Shortcuts.Action.Ring, Shortcuts.Device.KEYBOARD))
        val moved = Shortcuts.parse(
            """{"v":1,"bindings":[{"on":"pad","keys":["start","select"],"hold":true,"do":"end_stream"}]}""",
        )
        assertEquals(say(R.string.controls_hold, "Start + Select"), moved.hint(end, Shortcuts.Device.PAD))
        assertEquals(null, moved.hint(end, Shortcuts.Device.KEYBOARD))
    }

    /** A binding row edits its binding, a fixed row nothing; a TV lists its remote, a phone its
     *  touchscreen. */
    @Test
    fun rowsPointAtTheirBindings() {
        val d = Shortcuts.defaults(Shortcuts.Client.ANDROID, false, "on")
        val phone = d.rows(touchscreen = true)
        assertEquals(
            listOf(Shortcuts.GroupKind.KEYBOARD, Shortcuts.GroupKind.TOUCH, Shortcuts.GroupKind.PAD),
            phone.map { it.kind },
        )
        assertEquals(d.bindings.indices.toList(), phone.flatMap { it.rows }.mapNotNull { it.binding }.sorted())
        assertTrue(phone.first { it.kind == Shortcuts.GroupKind.TOUCH }.rows.all { it.binding == null && it.text != null })
        val tv = d.rows(touchscreen = false)
        assertEquals(Shortcuts.GroupKind.REMOTE, tv.first().kind)
        assertTrue(tv.none { it.kind == Shortcuts.GroupKind.TOUCH })
        val guide = phone.last().rows.first { it.press == say(R.string.controls_hold, "Select") }
        assertEquals(Shortcuts.Action.Slot(SlotId.Guide), d.bindings[guide.binding!!].action)
    }
}
