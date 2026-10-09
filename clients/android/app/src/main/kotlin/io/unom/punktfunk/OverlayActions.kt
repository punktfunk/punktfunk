package io.unom.punktfunk

import io.unom.punktfunk.kit.Gamepad
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/*
 * The in-stream quick-action ring's configuration — the `overlay_actions` setting, one JSON blob
 * (schema v2, design/touch-client-overlay.md §3.2). The Kotlin twin of pf-client-core's
 * `overlay_actions.rs`; `OverlayActionsTest` replays the shared contract,
 * `clients/shared/overlay-actions-vectors.json`.
 *
 * Parsing never fails: fewer than six slots pad with empty, more are truncated, an unknown id or a
 * dangling `shortcut:` reference is an empty slot, an absent field takes its default, and an
 * unparseable blob is the platform default — presets sync between client versions.
 */

/** What a ring slot does. [Host] carries a host-advertised action id; [Shortcut] refers into
 *  [OverlayConfig.shortcuts] by id. */
sealed class SlotId {
    object EndStream : SlotId()

    /** End the game this device launched, then the stream. */
    object EndGame : SlotId()
    object DisconnectLinger : SlotId()
    object TouchMode : SlotId()
    object Keyboard : SlotId()
    object Stats : SlotId()
    object Mic : SlotId()
    object Pad : SlotId()
    object SendText : SlotId()

    /** The host's guide button (Xbox / PS / Steam), as a synthetic pad tap. */
    object Guide : SlotId()

    /** The host's quick-access button — `BTN_MISC1`, the Deck's `…`. */
    object Qam : SlotId()

    /** Controller mouse: the pad drives the host pointer instead of its virtual pad. */
    object PadMouse : SlotId()

    /** Step the controller type the host emulates, live ([nextPadType]). */
    object PadType : SlotId()

    /** Silence this device's speakers. Local: the host keeps playing for anyone joined to it. */
    object StreamMute : SlotId()

    /** A dual-screen handheld's two screens trade the picture and the companion panel. */
    object SwapScreens : SlotId()
    data class Host(val actionId: String) : SlotId()
    data class Shortcut(val shortcutId: String) : SlotId()

    /** The wire id, the inverse of [parse]. */
    val id: String
        get() = when (this) {
            EndStream -> "end_stream"
            EndGame -> "end_game"
            DisconnectLinger -> "disconnect_linger"
            TouchMode -> "touch_mode"
            Keyboard -> "keyboard"
            Stats -> "stats"
            Mic -> "mic"
            Pad -> "pad"
            SendText -> "send_text"
            Guide -> "guide"
            Qam -> "qam"
            PadMouse -> "pad_mouse"
            PadType -> "pad_type"
            StreamMute -> "stream_mute"
            SwapScreens -> "swap_screens"
            is Host -> "host:$actionId"
            is Shortcut -> "shortcut:$shortcutId"
        }

    companion object {
        /** An id from the blob; `null` for one this build does not know (an empty slot). */
        fun parse(s: String): SlotId? = when (s) {
            "end_stream" -> EndStream
            "end_game" -> EndGame
            "disconnect_linger" -> DisconnectLinger
            "touch_mode" -> TouchMode
            "keyboard" -> Keyboard
            "stats" -> Stats
            "mic" -> Mic
            "pad" -> Pad
            "send_text" -> SendText
            "guide" -> Guide
            "qam" -> Qam
            "pad_mouse" -> PadMouse
            "pad_type" -> PadType
            "stream_mute" -> StreamMute
            "swap_screens" -> SwapScreens
            else -> when {
                s.startsWith("host:") && s.length > 5 -> Host(s.substring(5))
                s.startsWith("shortcut:") && s.length > 9 -> Shortcut(s.substring(9))
                else -> null
            }
        }
    }
}

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

/** The Controller type slot's state word. */
fun padTypeLabel(pref: Int): String = when (pref) {
    Gamepad.PREF_AUTO -> "Automatic"
    Gamepad.PREF_XBOX360 -> "Xbox 360"
    Gamepad.PREF_XBOXONE -> "Xbox One"
    Gamepad.PREF_DUALSENSE -> "DualSense"
    Gamepad.PREF_DUALSHOCK4 -> "DualShock 4"
    Gamepad.PREF_STEAMDECK -> "Steam Deck"
    else -> "Other"
}

/** A custom key chord. [keys] are names from the shared keymap tables, never raw key codes. */
data class Shortcut(val id: String, val label: String = "", val keys: List<String> = emptyList())

/**
 * The Windows virtual-key code a shortcut key name stands for (the wire speaks VKs); `null` for
 * a name this build does not know. Twin of the Rust `key_vk`: `OverlayActionsTest` replays
 * `crates/core/punktfunk-core/testdata/key-vk-vectors.json`, so a new name lands there first.
 */
fun keyVk(name: String): Int? {
    val n = name.trim().lowercase()
    return when (n) {
        "ctrl", "control" -> 0x11
        "shift" -> 0x10
        "alt", "option" -> 0x12
        "win", "cmd", "super", "meta" -> 0x5B
        "escape", "esc" -> 0x1B
        "tab" -> 0x09
        "enter", "return" -> 0x0D
        "space" -> 0x20
        "backspace" -> 0x08
        "delete", "del" -> 0x2E
        "insert" -> 0x2D
        "home" -> 0x24
        "end" -> 0x23
        "pageup" -> 0x21
        "pagedown" -> 0x22
        "up" -> 0x26
        "down" -> 0x28
        "left" -> 0x25
        "right" -> 0x27
        "printscreen" -> 0x2C
        "pause" -> 0x13
        "capslock" -> 0x14
        else -> when {
            n.length == 1 && n[0] in 'a'..'z' -> 0x41 + (n[0] - 'a')
            n.length == 1 && n[0] in '0'..'9' -> 0x30 + (n[0] - '0')
            n.length >= 2 && n[0] == 'f' ->
                n.substring(1).toIntOrNull()?.takeIf { it in 1..24 }?.let { 0x70 + it - 1 }
            else -> null
        }
    }
}

/** A chord as a legend reads it: `Ctrl+Shift+Esc`. */
fun chordChip(keys: List<String>): String = keys.joinToString("+", transform = ::keyLegend)

/**
 * One key's legend: the word a keyboard prints on it (`Ctrl`, `Esc`, `PgUp`), arrows as arrows.
 * Symbols like ❖ or ⇧ read as nothing to most people, so none are used here.
 */
fun keyLegend(k: String): String = when (k.trim().lowercase()) {
    "ctrl", "control" -> "Ctrl"
    "shift" -> "Shift"
    "alt", "option" -> "Alt"
    "win", "cmd", "super", "meta" -> "Win"
    "escape", "esc" -> "Esc"
    "enter", "return" -> "Enter"
    "backspace" -> "Backspace"
    "delete", "del" -> "Del"
    "insert" -> "Ins"
    "pageup" -> "PgUp"
    "pagedown" -> "PgDn"
    "printscreen" -> "PrtSc"
    "capslock" -> "Caps"
    "up" -> "↑"
    "down" -> "↓"
    "left" -> "←"
    "right" -> "→"
    else -> k.trim().lowercase().replaceFirstChar { it.uppercase() }
}

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
