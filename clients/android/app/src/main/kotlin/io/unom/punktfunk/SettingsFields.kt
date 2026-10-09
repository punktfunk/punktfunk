package io.unom.punktfunk

import android.content.SharedPreferences
import org.json.JSONObject

/**
 * The one list of [Settings] fields. Every place that walks the fields — the prefs store, the
 * preset overlay's apply/absorb/clear/JSON, the console document both ways — loops over this,
 * so a new field is one row here plus its two data-class properties. `SettingsFieldsTest`
 * checks by reflection that no property is missing from the table.
 */
internal object SettingsFields {
    /** The width/height pair, which one control drives — the reset alias, as on every client. */
    const val FIELD_RESOLUTION = "resolution"

    /** The console's stored spellings, by wire byte; `xboxelite` is a console-only row. */
    val GAMEPAD_NAMES = io.unom.punktfunk.kit.Gamepad.PREFS.map { it.name } + "xboxelite"
    val COMPOSITOR_NAMES = listOf("auto", "kwin", "wlroots", "mutter", "gamescope", "hyprland")

    val ALL: List<Field<*>> = listOf(
        field("width", "width", IntKind, { it.width }, { s, v -> s.copy(width = v) },
            overlay({ it.width }, { o, v -> o.copy(width = v) }), console = Console(ConsoleSizeKind)),
        field("height", "height", IntKind, { it.height }, { s, v -> s.copy(height = v) },
            overlay({ it.height }, { o, v -> o.copy(height = v) }), console = Console(ConsoleSizeKind)),
        field("hz", "refresh_hz", IntKind, { it.hz }, { s, v -> s.copy(hz = v) },
            overlay({ it.hz }, { o, v -> o.copy(hz = v) }), prefsKey = "hz"),
        // Qualifiers on the safe-area resolution. `android.` keys, so they ride the console
        // document's `Settings::extra` rather than needing a row in the shared shell.
        field("bitrateKbps", "bitrate_kbps", IntKind, { it.bitrateKbps }, { s, v -> s.copy(bitrateKbps = v) },
            overlay({ it.bitrateKbps }, { o, v -> o.copy(bitrateKbps = v) })),
        // Prefs keep a float; reading back in hundredths, the dial's unit, returns what was saved.
        field("pyrowaveBpp", "pyrowave_bpp", DoubleKind, { it.pyrowaveBpp },
            { s, v -> s.copy(pyrowaveBpp = Math.round(v * 100) / 100.0) },
            overlay({ it.pyrowaveBpp }, { o, v -> o.copy(pyrowaveBpp = v) })),
        field("renderScale", "render_scale", DoubleKind, { it.renderScale }, { s, v -> s.copy(renderScale = v) },
            overlay({ it.renderScale }, { o, v -> o.copy(renderScale = v) })),
        field("videoFit", "video_fit", StrKind, { it.videoFit }, { s, v -> s.copy(videoFit = v) },
            overlay({ it.videoFit }, { o, v -> o.copy(videoFit = v) })),
        // Android alone has a second screen to use; the `android.` key rides a preset's `extra`
        // on every other client.
        field("secondScreen", "android.second_screen", BoolKind, { it.secondScreen }, { s, v -> s.copy(secondScreen = v) },
            overlay({ it.secondScreen }, { o, v -> o.copy(secondScreen = v) }), prefsKey = "second_screen"),
        field("hdrEnabled", "hdr_enabled", BoolKind, { it.hdrEnabled }, { s, v -> s.copy(hdrEnabled = v) },
            overlay({ it.hdrEnabled }, { o, v -> o.copy(hdrEnabled = v) })),
        field("tenBitSdr", "ten_bit_sdr", BoolKind, { it.tenBitSdr }, { s, v -> s.copy(tenBitSdr = v) },
            overlay({ it.tenBitSdr }, { o, v -> o.copy(tenBitSdr = v) })),
        field("compositor", "compositor", IntKind, { it.compositor }, { s, v -> s.copy(compositor = v) },
            overlay({ it.compositor }, { o, v -> o.copy(compositor = v) }),
            console = Console(NamedIndexKind(COMPOSITOR_NAMES))),
        field("gamepad", "gamepad", IntKind, { it.gamepad }, { s, v -> s.copy(gamepad = v) },
            overlay({ it.gamepad }, { o, v -> o.copy(gamepad = v) }),
            console = Console(NamedIndexKind(GAMEPAD_NAMES))),
        field("gamepadForwarding", "gamepad_forwarding", BoolKind, { it.gamepadForwarding },
            { s, v -> s.copy(gamepadForwarding = v) },
            overlay({ it.gamepadForwarding }, { o, v -> o.copy(gamepadForwarding = v) })),
        field("systemButtons", "system_buttons", StrKind, { it.systemButtons }, { s, v -> s.copy(systemButtons = v) },
            overlay({ it.systemButtons }, { o, v -> o.copy(systemButtons = v) })),
        field("guideGesture", "guide_gesture", StrKind, { it.guideGesture }, { s, v -> s.copy(guideGesture = v) },
            overlay({ it.guideGesture }, { o, v -> o.copy(guideGesture = v) })),
        field("audioChannels", "audio_channels", IntKind, { it.audioChannels }, { s, v -> s.copy(audioChannels = v) },
            overlay({ it.audioChannels }, { o, v -> o.copy(audioChannels = v) })),
        field("audioFormat", "audio_format", StrKind, { it.audioFormat }, { s, v -> s.copy(audioFormat = v) },
            overlay({ it.audioFormat }, { o, v -> o.copy(audioFormat = v) })),
        field("codec", "codec", StrKind, { it.codec }, { s, v -> s.copy(codec = v) },
            overlay({ it.codec }, { o, v -> o.copy(codec = v) })),
        field("micEnabled", "mic_enabled", BoolKind, { it.micEnabled }, { s, v -> s.copy(micEnabled = v) },
            overlay({ it.micEnabled }, { o, v -> o.copy(micEnabled = v) })),
        field("echoCancel", "echo_cancel", BoolKind, { it.echoCancel }, { s, v -> s.copy(echoCancel = v) },
            overlay({ it.echoCancel }, { o, v -> o.copy(echoCancel = v) })),
        field("keepHostAudio", "keep_host_audio", BoolKind, { it.keepHostAudio }, { s, v -> s.copy(keepHostAudio = v) },
            overlay({ it.keepHostAudio }, { o, v -> o.copy(keepHostAudio = v) })),
        field("statsVerbosity", "stats_verbosity", StatsVerbosityKind, { it.statsVerbosity },
            { s, v -> s.copy(statsVerbosity = v) },
            overlay({ it.statsVerbosity }, { o, v -> o.copy(statsVerbosity = v) }),
            console = Console(EnumKind(StatsVerbosity.entries) { it.name.lowercase() })),
        field("advancedStats", "advanced_stats", BoolKind, { it.advancedStats }, { s, v -> s.copy(advancedStats = v) }),
        field("hudPlacement", "hud_placement", StrKind, { it.hudPlacement }, { s, v -> s.copy(hudPlacement = v) }),
        field("statsScalePct", "stats_scale_pct", IntKind, { it.statsScalePct }, { s, v -> s.copy(statsScalePct = v) }),
        field("exitHint", "exit_hint", BoolKind, { it.exitHint }, { s, v -> s.copy(exitHint = v) }),
        field("showAdvanced", "show_advanced", BoolKind, { it.showAdvanced }, { s, v -> s.copy(showAdvanced = v) }),
        field("touchMode", "touch_mode", TouchModeKind, { it.touchMode }, { s, v -> s.copy(touchMode = v) },
            overlay({ it.touchMode }, { o, v -> o.copy(touchMode = v) }),
            console = Console(EnumKind(TouchMode.entries) { it.name.lowercase() })),
        field("gamepadUiEnabled", "gamepad_ui_enabled", BoolKind, { it.gamepadUiEnabled },
            { s, v -> s.copy(gamepadUiEnabled = v) }),
        field("reduceUiResolution", "android.reduce_ui_resolution", BoolKind, { it.reduceUiResolution },
            { s, v -> s.copy(reduceUiResolution = v) }, prefsKey = "reduce_ui_resolution"),
        field("gamepadUiMode", "gamepad_ui_mode", StrKind, { it.gamepadUiMode }, { s, v -> s.copy(gamepadUiMode = v) }),
        field("uiPalette", "ui_palette", StrKind, { it.uiPalette }, { s, v -> s.copy(uiPalette = v) }),
        // Prefs key bumped to `_v2` to restart every install at the new default (ON); both stale
        // keys are abandoned unread. The `android.` console key is a top-level `Settings::extra` row.
        field("lowLatencyMode", "low_latency_mode", BoolKind, { it.lowLatencyMode }, { s, v -> s.copy(lowLatencyMode = v) },
            overlay({ it.lowLatencyMode }, { o, v -> o.copy(lowLatencyMode = v) }),
            prefsKey = "low_latency_mode_v2", console = Console(BoolKind, "android.low_latency")),
        field("presentPriority", "present_priority", StrKind, { it.presentPriority }, { s, v -> s.copy(presentPriority = v) },
            overlay({ it.presentPriority }, { o, v -> o.copy(presentPriority = v) })),
        field("smoothBuffer", "smooth_buffer", IntKind, { it.smoothBuffer }, { s, v -> s.copy(smoothBuffer = v) },
            overlay({ it.smoothBuffer }, { o, v -> o.copy(smoothBuffer = v) })),
        field("autoWakeEnabled", "auto_wake", BoolKind, { it.autoWakeEnabled }, { s, v -> s.copy(autoWakeEnabled = v) },
            prefsKey = "auto_wake_enabled"),
        field("backgroundKeepAlive", "background_keep_alive", BoolKind, { it.backgroundKeepAlive },
            { s, v -> s.copy(backgroundKeepAlive = v) }),
        field("backgroundTimeoutMinutes", "background_timeout_minutes", IntKind,
            { it.backgroundTimeoutMinutes }, { s, v -> s.copy(backgroundTimeoutMinutes = v) }),
        field("padRumble", "pad_rumble", BoolKind, { it.padRumble }, { s, v -> s.copy(padRumble = v) }),
        field("rumbleOnPhone", "android.rumble_on_phone", BoolKind, { it.rumbleOnPhone }, { s, v -> s.copy(rumbleOnPhone = v) },
            prefsKey = "rumble_on_phone"),
        field("gyroOnPhone", "android.gyro_on_phone", BoolKind, { it.gyroOnPhone }, { s, v -> s.copy(gyroOnPhone = v) },
            prefsKey = "gyro_on_phone"),
        field("dsCapture", "android.ds_capture", BoolKind, { it.dsCapture }, { s, v -> s.copy(dsCapture = v) },
            prefsKey = "ds_capture"),
        field("padHaptics", "pad_haptics", BoolKind, { it.padHaptics }, { s, v -> s.copy(padHaptics = v) }),
        field("padSpeaker", "pad_speaker", BoolKind, { it.padSpeaker }, { s, v -> s.copy(padSpeaker = v) },
            console = Console(PadSpeakerKind)),
        field("mouseMode", "mouse_mode", MouseModeKind, { it.mouseMode }, { s, v -> s.copy(mouseMode = v) },
            overlay({ it.mouseMode }, { o, v -> o.copy(mouseMode = v) })),
        field("invertScroll", "invert_scroll", BoolKind, { it.invertScroll }, { s, v -> s.copy(invertScroll = v) },
            overlay({ it.invertScroll }, { o, v -> o.copy(invertScroll = v) })),
        field("overlayActions", "overlay_actions", StrKind, { it.overlayActions }, { s, v -> s.copy(overlayActions = v) },
            overlay({ it.overlayActions }, { o, v -> o.copy(overlayActions = v) })),
        field("backOpensRing", "android.back_opens_ring", BoolKind, { it.backOpensRing },
            { s, v -> s.copy(backOpensRing = v) }, prefsKey = "back_opens_ring"),
        // Cross-client start-screen keys; the console writes the same two names.
        field("startIn", "start_in", StrKind, { it.startIn }, { s, v -> s.copy(startIn = v) }),
        field("defaultHost", "default_host", NullableStrKind, { it.defaultHost }, { s, v -> s.copy(defaultHost = v) }),
    )

    /** The presetable (tier-P) rows, in table order. */
    val PRESET: List<Field<*>> = ALL.filter { it.overlay != null }

    /** The overlay's JSON keys — everything else in a stored overlay is carried through. */
    val PRESET_KEYS: Set<String> = PRESET.map { it.key }.toSet()

    /** Legacy prefs keys, read once as a migration default and never written. */
    private const val K_HUD = "stats_hud_enabled"
    private const val K_TRACKPAD = "trackpad_mode"
    private const val K_POINTER_CAPTURE = "pointer_capture"

    /**
     * One field. [name] is the Kotlin property (the reflection check keys on it), [key] the JSON
     * key on the preset overlay and, unless [console] renames it, the console document;
     * [prefsKey] the SharedPreferences key when it differs from [key].
     */
    class Field<T>(
        val name: String,
        val key: String,
        val prefsKey: String,
        val kind: Kind<T>,
        val get: (Settings) -> T,
        val set: (Settings, T) -> Settings,
        val overlay: Overlay<T>?,
        val console: Console<T>?,
    ) {
        fun load(s: Settings, p: SharedPreferences): Settings = set(s, kind.read(p, prefsKey, get(s)))
        fun save(e: SharedPreferences.Editor, s: Settings) = kind.write(e, prefsKey, get(s))

        /** [base] under this field's override, if the overlay carries one. */
        fun applyOverlay(o: SettingsOverlay, base: Settings): Settings =
            overlay?.get?.invoke(o)?.let { set(base, it) } ?: base

        /** Record an override when the field moved between [before] and [after]. */
        fun absorb(o: SettingsOverlay, before: Settings, after: Settings): SettingsOverlay =
            if (overlay != null && get(after) != get(before)) overlay.set(o, get(after)) else o

        fun clear(o: SettingsOverlay): SettingsOverlay = overlay?.set?.invoke(o, null) ?: o
        fun isOverridden(o: SettingsOverlay): Boolean = overlay?.get?.invoke(o) != null
        fun overlayToJson(o: SettingsOverlay, j: JSONObject) {
            overlay?.get?.invoke(o)?.let { kind.write(j, key, it) }
        }
        fun overlayFromJson(o: SettingsOverlay, j: JSONObject): SettingsOverlay =
            overlay?.let { ov -> kind.read(j, key)?.let { ov.set(o, it) } } ?: o

        /** The override in the console document's encoding, under the console's key. */
        fun overlayToConsoleJson(o: SettingsOverlay, j: JSONObject) {
            overlay?.get?.invoke(o)?.let { consoleKind.write(j, consoleKey, it) }
        }

        /** The console's saved override for this field, set or cleared. */
        fun overlayFromConsoleJson(o: SettingsOverlay, j: JSONObject): SettingsOverlay =
            overlay?.set?.invoke(o, consoleKind.read(j, consoleKey)) ?: o

        private val consoleKey get() = console?.key ?: key
        private val consoleKind get() = console?.kind ?: kind
        fun consoleWrite(j: JSONObject, s: Settings) = consoleKind.write(j, consoleKey, get(s))
        fun consoleRead(j: JSONObject, s: Settings): Settings = set(s, consoleKind.apply(j, consoleKey, get(s)))
    }

    class Overlay<T>(val get: (SettingsOverlay) -> T?, val set: (SettingsOverlay, T?) -> SettingsOverlay)

    /** The console document's encoding of a field, when it differs from the overlay's. */
    class Console<T>(val kind: Kind<T>, val key: String? = null)

    /** How a value is stored in SharedPreferences and in JSON. */
    interface Kind<T> {
        fun read(p: SharedPreferences, k: String, def: T): T
        fun write(e: SharedPreferences.Editor, k: String, v: T)

        /** `null` = absent or unreadable. */
        fun read(j: JSONObject, k: String): T?
        fun write(j: JSONObject, k: String, v: T)

        /** The console's read: an unreadable value snaps to [cur], never corrupting the store. */
        fun apply(j: JSONObject, k: String, cur: T): T = read(j, k) ?: cur
    }

    object IntKind : Kind<Int> {
        override fun read(p: SharedPreferences, k: String, def: Int) = p.getInt(k, def)
        override fun write(e: SharedPreferences.Editor, k: String, v: Int) { e.putInt(k, v) }
        override fun read(j: JSONObject, k: String): Int? = if (j.has(k)) j.optInt(k) else null
        override fun write(j: JSONObject, k: String, v: Int) { j.put(k, v) }
    }

    /**
     * `width`/`height` on the console, whose sizes are unsigned: one negative value fails the
     * whole document. [SAFE_AREA_MODE] travels as `0` plus `android.safe_area_mode`.
     */
    object ConsoleSizeKind : Kind<Int> {
        const val SAFE_AREA_KEY = "android.safe_area_mode"
        override fun read(p: SharedPreferences, k: String, def: Int) = p.getInt(k, def)
        override fun write(e: SharedPreferences.Editor, k: String, v: Int) { e.putInt(k, v) }
        override fun read(j: JSONObject, k: String): Int? = if (!j.has(k)) null else j.optInt(k).let {
            if (it == 0 && j.optBoolean(SAFE_AREA_KEY)) SAFE_AREA_MODE else it
        }
        override fun write(j: JSONObject, k: String, v: Int) {
            j.put(k, v.coerceAtLeast(0))
            j.put(SAFE_AREA_KEY, v == SAFE_AREA_MODE)
        }
    }

    object BoolKind : Kind<Boolean> {
        override fun read(p: SharedPreferences, k: String, def: Boolean) = p.getBoolean(k, def)
        override fun write(e: SharedPreferences.Editor, k: String, v: Boolean) { e.putBoolean(k, v) }
        override fun read(j: JSONObject, k: String): Boolean? = if (j.has(k)) j.optBoolean(k) else null
        override fun write(j: JSONObject, k: String, v: Boolean) { j.put(k, v) }
    }

    /** Stored as a Float in prefs (the original key type), a Double in JSON. */
    object DoubleKind : Kind<Double> {
        override fun read(p: SharedPreferences, k: String, def: Double) = p.getFloat(k, def.toFloat()).toDouble()
        override fun write(e: SharedPreferences.Editor, k: String, v: Double) { e.putFloat(k, v.toFloat()) }
        override fun read(j: JSONObject, k: String): Double? = if (j.has(k)) j.optDouble(k) else null
        override fun write(j: JSONObject, k: String, v: Double) { j.put(k, v) }
    }

    /** An empty string reads as absent: the overlay never pins one, and the console keeps the current value. */
    object StrKind : Kind<String> {
        override fun read(p: SharedPreferences, k: String, def: String) = p.getString(k, def) ?: def
        override fun write(e: SharedPreferences.Editor, k: String, v: String) { e.putString(k, v) }
        override fun read(j: JSONObject, k: String): String? = if (j.has(k)) j.optString(k).ifEmpty { null } else null
        override fun write(j: JSONObject, k: String, v: String) { j.put(k, v) }
    }

    /** `default_host`: absent (or empty) IS the value — the console omits it when no host is chosen. */
    object NullableStrKind : Kind<String?> {
        override fun read(p: SharedPreferences, k: String, def: String?) = p.getString(k, def)
        override fun write(e: SharedPreferences.Editor, k: String, v: String?) { e.putString(k, v) }
        override fun read(j: JSONObject, k: String): String? = j.optString(k, "").ifEmpty { null }
        override fun write(j: JSONObject, k: String, v: String?) { if (v != null) j.put(k, v) else j.remove(k) }
        override fun apply(j: JSONObject, k: String, cur: String?): String? = read(j, k)
    }

    /** An enum by its stored spelling; an unknown spelling reads as absent. */
    open class EnumKind<E : Enum<E>>(private val entries: List<E>, private val stored: (E) -> String) : Kind<E> {
        override fun read(p: SharedPreferences, k: String, def: E): E = p.getString(k, null)?.let(::parse) ?: def
        override fun write(e: SharedPreferences.Editor, k: String, v: E) { e.putString(k, stored(v)) }
        override fun read(j: JSONObject, k: String): E? = if (j.has(k)) parse(j.optString(k)) else null
        override fun write(j: JSONObject, k: String, v: E) { j.put(k, stored(v)) }
        protected fun parse(n: String): E? = entries.firstOrNull { stored(it) == n }
    }

    /**
     * Migration from the pre-tier Boolean `stats_hud_enabled`: an explicit OFF stays off;
     * everyone else (incl. fresh installs) lands on NORMAL — the old always-full HUD toned down
     * to the new default, which is the whole point of adding tiers.
     */
    object StatsVerbosityKind : EnumKind<StatsVerbosity>(StatsVerbosity.entries, { it.name }) {
        override fun read(p: SharedPreferences, k: String, def: StatsVerbosity): StatsVerbosity =
            p.getString(k, null)?.let(::parse)
                ?: if (p.contains(K_HUD) && !p.getBoolean(K_HUD, true)) StatsVerbosity.OFF else StatsVerbosity.NORMAL
    }

    /** Migration: the pre-enum Boolean `trackpad_mode` (true = trackpad, false = direct). */
    object TouchModeKind : EnumKind<TouchMode>(TouchMode.entries, { it.name }) {
        override fun read(p: SharedPreferences, k: String, def: TouchMode): TouchMode =
            p.getString(k, null)?.let(::parse)
                ?: if (p.getBoolean(K_TRACKPAD, true)) TouchMode.TRACKPAD else TouchMode.POINTER
    }

    /**
     * Migration: the pre-enum Boolean `pointer_capture` (true = lock the pointer). Its default
     * was false, which IS `desktop` — an install that never touched the toggle lands where it was.
     */
    object MouseModeKind : EnumKind<MouseMode>(MouseMode.entries, { it.storedName }) {
        override fun read(p: SharedPreferences, k: String, def: MouseMode): MouseMode =
            p.getString(k, null)?.let(::parse)
                ?: if (p.getBoolean(K_POINTER_CAPTURE, false)) MouseMode.CAPTURE else MouseMode.DESKTOP
    }

    /** The console spells an index field by name; an unknown name reads as absent. */
    class NamedIndexKind(private val names: List<String>) : Kind<Int> {
        override fun read(p: SharedPreferences, k: String, def: Int) = p.getInt(k, def)
        override fun write(e: SharedPreferences.Editor, k: String, v: Int) { e.putInt(k, v) }
        override fun read(j: JSONObject, k: String): Int? = names.indexOf(j.optString(k)).takeIf { it >= 0 }
        override fun write(j: JSONObject, k: String, v: Int) { j.put(k, names.getOrElse(v) { "auto" }) }
    }

    /**
     * `pad_speaker` on the console: `"pad"` = on, `"off"` = off. `"mix"` is off, not on: it is
     * unimplemented everywhere and `pad_audio::speaker_active` renders it as off, so a preset
     * carrying it must not open the pad's speaker here alone.
     */
    object PadSpeakerKind : Kind<Boolean> {
        override fun read(p: SharedPreferences, k: String, def: Boolean) = p.getBoolean(k, def)
        override fun write(e: SharedPreferences.Editor, k: String, v: Boolean) { e.putBoolean(k, v) }
        override fun read(j: JSONObject, k: String): Boolean? = when (j.optString(k, "")) {
            "pad" -> true
            "mix", "off" -> false
            else -> null
        }
        override fun write(j: JSONObject, k: String, v: Boolean) { j.put(k, if (v) "pad" else "off") }
    }

    private fun <T> field(
        name: String,
        key: String,
        kind: Kind<T>,
        get: (Settings) -> T,
        set: (Settings, T) -> Settings,
        overlay: Overlay<T>? = null,
        prefsKey: String = key,
        console: Console<T>? = null,
    ) = Field(name, key, prefsKey, kind, get, set, overlay, console)

    private fun <T> overlay(get: (SettingsOverlay) -> T?, set: (SettingsOverlay, T?) -> SettingsOverlay) =
        Overlay(get, set)
}
