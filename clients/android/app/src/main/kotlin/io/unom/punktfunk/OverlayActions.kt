package io.unom.punktfunk

import android.content.Context
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.SlotId
import io.unom.punktfunk.kit.keyVk
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/*
 * The in-stream quick-action ring's configuration — the `overlay_actions` setting, one JSON blob
 * (schema v2, design/touch-client-overlay.md §3.2). The Kotlin twin of pf-client-core's
 * `overlay_actions.rs`; `OverlayActionsTest` replays the shared contract,
 * `clients/shared/overlay-actions-vectors.json`. The slot ids and key names are the kit's
 * ([SlotId], [keyVk]), which the shortcuts share.
 *
 * Parsing never fails: fewer than six slots pad with empty, more are truncated, an unknown id or a
 * dangling `shortcut:` reference is an empty slot, an absent field takes its default, and an
 * unparseable blob is the platform default — presets sync between client versions.
 */

/** What the Controller type slot steps through, as GamepadPref wire bytes: Automatic, then the
 *  pads every host builds. Twin of the Rust `PAD_TYPE_CYCLE`. */
val PAD_TYPE_CYCLE = intArrayOf(
    Gamepad.PREF_AUTO, Gamepad.PREF_XBOX360, Gamepad.PREF_XBOXONE,
    Gamepad.PREF_DUALSENSE, Gamepad.PREF_DUALSHOCK4, Gamepad.PREF_STEAMDECK,
)

/** The type after [current]; one outside the cycle (picked in Settings) steps to Automatic. */
fun nextPadType(current: Int): Int {
    val at = PAD_TYPE_CYCLE.indexOf(current)
    return if (at < 0) Gamepad.PREF_AUTO else PAD_TYPE_CYCLE[(at + 1) % PAD_TYPE_CYCLE.size]
}

/** The Controller type slot's state word: a brand name, or a message for Automatic and the rest. */
fun padTypeLabel(context: Context, pref: Int): String = when (pref) {
    Gamepad.PREF_AUTO -> context.getString(R.string.ring_pad_type_automatic)
    Gamepad.PREF_XBOX360 -> "Xbox 360"
    Gamepad.PREF_XBOXONE -> "Xbox One"
    Gamepad.PREF_DUALSENSE -> "DualSense"
    Gamepad.PREF_DUALSHOCK4 -> "DualShock 4"
    Gamepad.PREF_STEAMDECK -> "Steam Deck"
    else -> context.getString(R.string.ring_pad_type_other)
}

/** The message naming a statistics tier, for the ring and the companion panel. */
internal fun statsVerbosityLabel(tier: StatsVerbosity): Int = when (tier) {
    StatsVerbosity.OFF -> R.string.player_stats_verbosity_off
    StatsVerbosity.COMPACT -> R.string.player_stats_verbosity_compact
    StatsVerbosity.NORMAL -> R.string.player_stats_verbosity_normal
    StatsVerbosity.DETAILED -> R.string.player_stats_verbosity_detailed
}

/** A key combination (`shortcut:<id>`). [keys] are [keyVk] names, never raw key codes; a
 *  modifier alone is one (`["win"]` opens the Start menu). */
data class Shortcut(val id: String, val label: String = "", val keys: List<String> = emptyList())

/**
 * One control's layout override: [x] and [y] place its centre as fractions of the layer's width
 * and height, [scale] sizes it about that centre, [hidden] takes it out of the stream (the
 * editor still shows it, ghosted). An absent field keeps the preset's value.
 */
data class PadTweak(val x: Float? = null, val y: Float? = null, val scale: Float? = null, val hidden: Boolean = false)

/**
 * The virtual controller: [layout] is `full`, `sticks` or `dpad`; [opacity] and [scale] are the
 * two sliders. [controls] carries the per-control overrides keyed by control id (`ls`, `rs`,
 * `dpad`, `face`, `lb`, `rb`, `lt`, `rt`, `select`, `guide`, `start`), [controlsNarrow] the
 * same for a narrow (upright) layer — the two classes the preset already lays out differently.
 */
data class PadConfig(
    val layout: String = "full",
    val opacity: Float = 0.45f,
    val scale: Float = 1f,
    val controls: Map<String, PadTweak> = emptyMap(),
    val controlsNarrow: Map<String, PadTweak> = emptyMap(),
)

/** Which platform default ring applies. Android is always [TOUCH]; [DESKTOP] exists so the
 *  parser matches its twins exactly. */
enum class RingPlatform { TOUCH, DESKTOP }

data class OverlayConfig(
    /** Exactly [RING_SLOTS] entries, clockwise from 12 o'clock; `null` is an empty slot. */
    val ring: List<SlotId?>,
    val shortcuts: List<Shortcut> = emptyList(),
    val pad: PadConfig = PadConfig(),
) {
    fun shortcut(id: String): Shortcut? = shortcuts.firstOrNull { it.id == id }

    /** The blob to store — always the current schema version. */
    fun toJson(): String {
        val j = JSONObject()
        j.put("v", SCHEMA_VERSION)
        j.put("ring", JSONArray().also { arr -> ring.forEach { arr.put(it?.id ?: JSONObject.NULL) } })
        j.put(
            "shortcuts",
            JSONArray().also { arr ->
                shortcuts.forEach { s ->
                    arr.put(
                        JSONObject().put("id", s.id).put("label", s.label)
                            .put("keys", JSONArray(s.keys)),
                    )
                }
            },
        )
        val padOut = JSONObject().put("layout", pad.layout).put("opacity", pad.opacity.toDouble())
            .put("scale", pad.scale.toDouble())
        // An untouched pad keeps its blob clean: no empty maps, no absent fields as nulls.
        fun tweaks(map: Map<String, PadTweak>) = JSONObject().also { out ->
            map.forEach { (id, t) ->
                out.put(
                    id,
                    JSONObject().apply {
                        t.x?.let { put("x", it.toDouble()) }
                        t.y?.let { put("y", it.toDouble()) }
                        t.scale?.let { put("scale", it.toDouble()) }
                        if (t.hidden) put("hidden", true)
                    },
                )
            }
        }
        if (pad.controls.isNotEmpty()) padOut.put("controls", tweaks(pad.controls))
        if (pad.controlsNarrow.isNotEmpty()) padOut.put("controls_narrow", tweaks(pad.controlsNarrow))
        j.put("pad", padOut)
        return j.toString()
    }

    companion object {
        const val RING_SLOTS = 6
        const val SCHEMA_VERSION = 2

        fun platformDefault(platform: RingPlatform = RingPlatform.TOUCH): OverlayConfig = OverlayConfig(
            ring = when (platform) {
                RingPlatform.TOUCH -> listOf(
                    SlotId.EndStream, SlotId.Keyboard, SlotId.TouchMode,
                    SlotId.Stats, SlotId.Mic, SlotId.Pad,
                )
                RingPlatform.DESKTOP -> listOf(
                    SlotId.EndStream, SlotId.DisconnectLinger, SlotId.TouchMode,
                    SlotId.Stats, SlotId.Mic, SlotId.SendText,
                )
            },
        )

        /** Parse the setting; an empty or unparseable blob is the platform default. */
        fun parse(json: String?, platform: RingPlatform = RingPlatform.TOUCH): OverlayConfig {
            if (json.isNullOrBlank()) return platformDefault(platform)
            val j = try {
                JSONObject(json)
            } catch (_: JSONException) {
                return platformDefault(platform)
            }
            val shortcuts = buildList {
                val arr = j.optJSONArray("shortcuts") ?: JSONArray()
                for (i in 0 until arr.length()) {
                    val s = arr.optJSONObject(i) ?: continue
                    val id = s.optString("id", "")
                    if (id.isEmpty()) continue
                    val keys = s.optJSONArray("keys")?.let { k ->
                        (0 until k.length()).map { k.optString(it) }
                    } ?: emptyList()
                    add(Shortcut(id, s.optString("label", ""), keys))
                }
            }
            val ringIn = j.optJSONArray("ring") ?: JSONArray()
            val ring = (0 until RING_SLOTS).map { i ->
                if (i >= ringIn.length() || ringIn.isNull(i)) return@map null
                SlotId.parse(ringIn.optString(i))?.takeIf { slot ->
                    slot !is SlotId.Shortcut || shortcuts.any { it.id == slot.shortcutId }
                }
            }
            val padIn = j.optJSONObject("pad")
            fun tweaks(key: String): Map<String, PadTweak> {
                val obj = padIn?.optJSONObject(key) ?: return emptyMap()
                return buildMap {
                    obj.keys().forEach { id ->
                        val t = obj.optJSONObject(id) ?: return@forEach
                        put(
                            id,
                            PadTweak(
                                x = t.optDouble("x").takeIf { !it.isNaN() }?.toFloat(),
                                y = t.optDouble("y").takeIf { !it.isNaN() }?.toFloat(),
                                scale = t.optDouble("scale").takeIf { !it.isNaN() }?.toFloat(),
                                hidden = t.optBoolean("hidden", false),
                            ),
                        )
                    }
                }
            }
            val pad = PadConfig(
                layout = padIn?.optString("layout", "full")?.ifEmpty { "full" } ?: "full",
                opacity = padIn?.optDouble("opacity", 0.45)?.toFloat() ?: 0.45f,
                scale = padIn?.optDouble("scale", 1.0)?.toFloat() ?: 1f,
                controls = tweaks("controls"),
                controlsNarrow = tweaks("controls_narrow"),
            )
            return OverlayConfig(ring, shortcuts, pad)
        }
    }
}
