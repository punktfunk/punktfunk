package io.unom.punktfunk.kit.library

import android.content.res.Resources
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.say
import org.json.JSONObject

/**
 * The stream settings a host sets for one game (the entry's `overrides`). A null field means the
 * device decides; [name] is the shared override the values came from. Applied at the connect that
 * launches the game. The twin of `punktfunk_core::game_overrides`; [fit] runs
 * `clients/shared/game-override-vectors.json`.
 */
data class GameOverrides(
    val name: String? = null,
    /** Height in lines; the width follows the requested mode's aspect. */
    val lines: Int? = null,
    val hz: Int? = null,
    val hdr: Boolean? = null,
    /** A pad name as `GamepadPref::from_name` reads it (`dualsense`). */
    val gamepad: String? = null,
    /** [lines] also reaches a panel with fewer lines, decoded and scaled down. */
    val beyondScreen: Boolean = false,
) {
    /** No field set: nothing to apply or show. A name alone sets nothing. */
    val isEmpty: Boolean
        get() = lines == null && hz == null && hdr == null && gamepad == null && !beyondScreen

    /**
     * [gamepad] as a [Gamepad] wire byte; null when unset or not a name in [Gamepad.PREFS].
     * ponytail: canonical names only, as the host stores them; aliases would need `from_name`'s table.
     */
    val gamepadPref: Int?
        get() = gamepad?.trim()?.lowercase()?.let { g -> Gamepad.PREFS.firstOrNull { it.name == g }?.wire }

    /**
     * The mode to ask for once these apply to [mode] (`w, h, hz`). [lines] replaces the height,
     * capped at [native]'s unless [beyondScreen]; [hz] is capped at [native]'s. A zero in
     * [native] caps nothing. Then both axes scale down together to [codecMax].
     */
    fun fit(mode: Triple<Int, Int, Int>, native: Triple<Int, Int, Int>, codecMax: Int): Triple<Int, Int, Int> {
        var (w, h, rate) = mode
        lines?.let { asked ->
            val target = if (beyondScreen || native.second == 0) asked else minOf(asked, native.second)
            if (target != mode.second && mode.second > 0) {
                w = even(mode.first.toDouble() * target / mode.second)
                h = even(target.toDouble())
            }
        }
        hz?.let { rate = if (native.third == 0) it else minOf(it, native.third) }
        val longest = maxOf(w, h)
        if (longest > codecMax && longest > 0) {
            val s = codecMax.toDouble() / longest
            w = minOf(even(w * s), codecMax)
            h = minOf(even(h * s), codecMax)
        }
        return Triple(w, h, rate)
    }

    /** What the host set, for "Host: …": the shared override's name, else `1080p60`, `HDR off`, the pad. */
    val summary: List<Say>
        get() = name?.let { listOf(Say.Text(it)) } ?: listOfNotNull(
            when {
                lines != null -> Say.Text("${lines}p${hz ?: ""}")
                hz != null -> say(R.string.common_hz, hz)
                else -> null
            },
            hdr?.let { say(if (it) R.string.lib_host_override_hdr_on else R.string.lib_host_override_hdr_off) },
            gamepadPref?.let { if (it == Gamepad.PREF_AUTO) say(R.string.player_gamepad_auto) else Say.Text(Gamepad.prefLabel(it)) },
        )

    /** [summary] worded and joined, or null when it names nothing this build can show. */
    fun summaryWords(res: Resources): String? =
        summary.takeIf { it.isNotEmpty() }?.joinToString(", ") { it.words(res) }

    fun toJson(): JSONObject = JSONObject().apply {
        name?.let { put("name", it) }
        lines?.let { put("lines", it) }
        hz?.let { put("hz", it) }
        hdr?.let { put("hdr", it) }
        gamepad?.let { put("gamepad", it) }
        if (beyondScreen) put("beyond_screen", true)
    }

    companion object {
        /** The wire object, tolerant of wrong types and unknown keys; null when it sets nothing. */
        fun from(o: JSONObject?): GameOverrides? = o?.let {
            GameOverrides(
                name = text(it, "name"),
                lines = (it.opt("lines") as? Number)?.toInt()?.takeIf { v -> v > 0 },
                hz = (it.opt("hz") as? Number)?.toInt()?.takeIf { v -> v > 0 },
                hdr = it.opt("hdr") as? Boolean,
                gamepad = text(it, "gamepad"),
                beyondScreen = it.opt("beyond_screen") == true,
            ).takeUnless { g -> g.isEmpty }
        }

        private fun text(o: JSONObject, key: String): String? = (o.opt(key) as? String)?.ifBlank { null }

        /** Nearest even number, ties up as core's `f64::round`; the host rejects odd sizes. */
        internal fun even(x: Double): Int = Math.round(x / 2.0).toInt() * 2
    }
}
