package io.unom.punktfunk

import android.content.Context
import android.content.SharedPreferences
import io.unom.punktfunk.kit.security.KnownHost
import java.security.SecureRandom
import org.json.JSONObject

/**
 * Client settings presets — named bundles of setting overrides applied on top of the global
 * [Settings] (design/client-settings-profiles.md §4). The Kotlin mirror of
 * `crates/pf-client-core/src/presets.rs`; the model is the same on every client, so get it right
 * here rather than re-deciding it.
 *
 * A preset overrides only the fields the user touched; everything else keeps following the global
 * defaults **live**, so fixing a global once fixes it everywhere. That is why an overlay is sparse
 * nullable fields rather than a snapshot copy, and why a value is written on touch and cleared only
 * on an explicit "reset to default" — never by diffing against the current global at save time. A
 * stored value that happens to equal today's global is a legitimate *pin*: the preset keeps it
 * when the global later moves.
 *
 * Only tier-P settings are here. Device facts (which pad this device forwards, whether its console
 * UI is on) and host facts (clipboard sync, which lives on the host record) are deliberately absent
 * — see the design's §3 curation.
 *
 * Values are stored exactly as [SettingsStore] persists them — ints for the compositor/gamepad wire
 * bytes, enum names for the rest — so there is one encoding of a setting on this platform rather
 * than two. The catalog is client-local (v1 has no preset sync or export), so nothing else reads
 * it.
 */
data class SettingsOverlay(
    val width: Int? = null,
    val height: Int? = null,
    val hz: Int? = null,
    val bitrateKbps: Int? = null,
    val pyrowaveBpp: Double? = null,
    val renderScale: Double? = null,
    val videoFit: String? = null,
    /** Android's second screen (a dual-screen handheld's lower panel): off for a preset that
     *  plays on a TV the size rule mistakes for one. */
    val secondScreen: Boolean? = null,
    val codec: String? = null,
    val hdrEnabled: Boolean? = null,
    val tenBitSdr: Boolean? = null,
    val compositor: Int? = null,
    val audioChannels: Int? = null,
    /**
     * The requested audio format ([AUDIO_FORMAT_OPTIONS]'s stored value). Presetable because it
     * is about how a HOST is streamed — a wired desktop can afford lossless, a phone on a hotspot
     * cannot — rather than about this device's hardware.
     */
    val audioFormat: String? = null,
    val micEnabled: Boolean? = null,
    val echoCancel: Boolean? = null,
    val keepHostAudio: Boolean? = null,
    val touchMode: TouchMode? = null,
    val mouseMode: MouseMode? = null,
    val invertScroll: Boolean? = null,
    /** The whole ring blob (design/touch-client-overlay.md D10): a preset inherits the default
     *  ring entirely or owns its own ring and shortcuts. */
    val overlayActions: String? = null,
    val gamepad: Int? = null,
    val gamepadForwarding: Boolean? = null,
    val systemButtons: String? = null,
    val guideGesture: String? = null,
    val statsVerbosity: StatsVerbosity? = null,
    /**
     * Android-only tier-P addition (design §3): the decode pipeline is a device fact everywhere
     * else, but here it is the one knob a marginal link wants turned off per host.
     */
    val lowLatencyMode: Boolean? = null,
    /** The timeline presenter's intent pair — cross-client keys, see [Settings.presentPriority]. */
    val presentPriority: String? = null,
    val smoothBuffer: Int? = null,
    /**
     * Overlay keys a newer build wrote and this one doesn't model — carried through a load→save
     * round-trip untouched. The don't-clobber rule: opening and saving a preset on an older client
     * must not erase what a newer one stored.
     */
    val extra: Map<String, Any> = emptyMap(),
) {
    /** The one resolution seam: this overlay on top of [base]. Pure, so it is fully testable. */
    fun apply(base: Settings): Settings =
        SettingsFields.PRESET.fold(base) { s, f -> f.applyOverlay(this, s) }

    /**
     * Record one control's value in [from] as its override — even one equal to today's global,
     * which is the pin. [FIELD_RESOLUTION] is the one alias, covering the width/height pair a single
     * control drives. An unknown name is a no-op. Removing an override is [clear].
     */
    fun pin(field: String, from: Settings): SettingsOverlay =
        SettingsFields.controlRows(field).fold(this) { o, f -> f.pin(o, from) }

    /** Drop one control's override, putting the row back to inheriting. Same names as [pin]. */
    fun clear(field: String): SettingsOverlay =
        SettingsFields.controlRows(field).fold(this) { o, f -> f.clear(o) }

    /** The field names this overlay overrides — what the settings rows draw their markers from. */
    fun overridden(): Set<String> = SettingsFields.PRESET
        .filter { it.isOverridden(this) }
        .map { if (it.key == "width" || it.key == "height") FIELD_RESOLUTION else it.key }
        .toSet()

    /**
     * True when the preset overrides nothing — "inherits everything", the state a freshly created
     * preset starts in. A preset holding only a newer build's field is NOT empty.
     */
    fun isEmpty(): Boolean = overridden().isEmpty() && extra.isEmpty()

    internal fun toJson(): JSONObject {
        val j = JSONObject()
        // Unknown keys first, so a modelled field always wins over a stale carried-through one.
        extra.forEach { (k, v) -> j.put(k, v) }
        SettingsFields.PRESET.forEach { it.overlayToJson(this, j) }
        return j
    }

    /** The overrides as the console document spells them — what its settings rows draw
     *  their preset markers from. Modelled fields only. */
    internal fun toConsoleJson(): JSONObject {
        val j = JSONObject()
        SettingsFields.PRESET.forEach { it.overlayToConsoleJson(this, j) }
        return j
    }

    companion object {
        /** The width/height pair, which one control drives — the reset alias, as on every client. */
        const val FIELD_RESOLUTION = SettingsFields.FIELD_RESOLUTION

        /** The console's saved overlay over [base]: each modelled field from [j], set or
         *  cleared; carried-through keys stay. */
        internal fun fromConsoleJson(j: JSONObject, base: SettingsOverlay): SettingsOverlay =
            SettingsFields.PRESET.fold(base) { o, f -> f.overlayFromConsoleJson(o, j) }

        internal fun fromJson(j: JSONObject): SettingsOverlay {
            // Keys this build models are read below; everything else is carried through.
            val extra = j.keys().asSequence()
                .filter { it !in SettingsFields.PRESET_KEYS }
                .associateWith { j.get(it) }
            return SettingsFields.PRESET.fold(SettingsOverlay(extra = extra)) { o, f -> f.overlayFromJson(o, j) }
        }
    }
}

/**
 * One named bundle of overrides. [id] is stable across renames — host bindings, pinned cards and
 * `punktfunk://` links all point at it, never at the name.
 */
data class StreamPreset(
    val id: String,
    /** User-facing and editable; unique case-insensitively (menus are ambiguous otherwise). */
    val name: String,
    /** `#RRGGBB` chip colour. Reserved by the schema; pinned cards tint their subtitle with it. */
    val accent: String? = null,
    val overrides: SettingsOverlay = SettingsOverlay(),
    /** Preset keys a newer build wrote — preserved across a load→save round-trip. */
    val extra: Map<String, Any> = emptyMap(),
)

/** What a `preset=` / one-off reference resolved to. Ambiguity is reported, never guessed. */
enum class PresetResolution { FOUND, NOT_FOUND, AMBIGUOUS }

/**
 * The preset catalog — client-wide, not per host: "Work" applied to three hosts is one preset,
 * and the per-host part is only the binding on the host record ([KnownHost.presetId]).
 *
 * Stored one JSON string per preset keyed by id in its own `punktfunk_presets` prefs file — the
 * `KnownHostStore` pattern, and deliberately not inside the settings file, which is rewritten
 * wholesale by several writers.
 */
class PresetStore(context: Context) {
    private val prefs = open(context.applicationContext)

    /** Every preset, name-sorted — the order the scope switcher and the menus show. */
    fun all(): List<StreamPreset> = prefs.all.values
        .mapNotNull { (it as? String)?.let(::parse) }
        .sortedBy { it.name.lowercase() }

    /** `as? String`: the file also holds a Boolean flag, and `id` can come from a link. */
    fun byId(id: String): StreamPreset? = (prefs.all[id] as? String)?.let(::parse)

    fun save(preset: StreamPreset) {
        prefs.edit().putString(preset.id, encode(preset)).apply()
    }

    fun delete(id: String) {
        prefs.edit().remove(id).apply()
    }

    /**
     * Resolve a reference the way every surface must: exact id first, then a unique
     * case-insensitive name. Two presets sharing a name resolve to [PresetResolution.AMBIGUOUS]
     * — a link or a flag naming two presets must refuse, not pick whichever came first.
     */
    fun resolve(reference: String): Pair<StreamPreset?, PresetResolution> {
        if (reference.isEmpty()) return null to PresetResolution.NOT_FOUND
        byId(reference)?.let { return it to PresetResolution.FOUND }
        val hits = all().filter { it.name.equals(reference, ignoreCase = true) }
        return when (hits.size) {
            1 -> hits[0] to PresetResolution.FOUND
            0 -> null to PresetResolution.NOT_FOUND
            else -> null to PresetResolution.AMBIGUOUS
        }
    }

    /**
     * Is this name already used (case-insensitively) by a *different* preset? The create/rename
     * guard — [except] is the preset being renamed, so renaming "Work" to "work" is allowed.
     */
    fun nameTaken(name: String, except: String? = null): Boolean =
        all().any { it.name.equals(name, ignoreCase = true) && it.id != except }

    /**
     * The preset a connect to [host] should use: the one-off pick, else the host's binding, else
     * none. [oneOff] is a reference (id or unique name); the empty string means "force the global
     * defaults" — a real choice ("Connect with ▸ Default settings" on a bound host), not "unset",
     * which is why it must survive as a value all the way down here. A binding whose preset was
     * deleted resolves as none: never an error, never a blocked connect.
     */
    fun resolveFor(host: KnownHost?, oneOff: String?, launch: String? = null): StreamPreset? =
        when {
            oneOff != null -> resolve(oneOff).first
            // A title's own binding is the more specific answer to the same question; a
            // deleted one falls through to the host's default, not past it to the globals.
            else -> launch?.let { host?.gamePresets?.get(it) }?.let(::byId)
                ?: host?.presetId?.let(::byId)
        }

    /** [host]'s pinned presets, in card order, with duplicates and deleted presets dropped. */
    fun pinsFor(host: KnownHost): List<StreamPreset> =
        host.pinnedPresetIds.distinct().mapNotNull(::byId)

    private fun parse(s: String): StreamPreset? = runCatching {
        val j = JSONObject(s)
        StreamPreset(
            id = j.getString("id"),
            name = j.getString("name"),
            accent = j.optStringOrNull("accent"),
            overrides = SettingsOverlay.fromJson(j.optJSONObject("overrides") ?: JSONObject()),
            extra = j.keys().asSequence()
                .filter { it !in setOf("id", "name", "accent", "overrides") }
                .associateWith { j.get(it) },
        )
    }.getOrNull()

    private fun encode(p: StreamPreset): String {
        val j = JSONObject()
        p.extra.forEach { (k, v) -> j.put(k, v) }
        j.put("id", p.id)
        j.put("name", p.name)
        p.accent?.let { j.put("accent", it) }
        j.put("overrides", p.overrides.toJson())
        return j.toString()
    }

    private companion object {
        const val PREFS = "punktfunk_presets"

        /** The pre-rename file (design/preset-rename.md), left as it was for an older build. */
        const val LEGACY_PREFS = "punktfunk_profiles"

        /** Marks the copy done, so deleting every preset does not bring the old ones back. A
         *  Boolean, which [all] skips: only String values are presets. */
        const val K_ADOPTED = "adopted_legacy"

        fun open(app: Context): SharedPreferences {
            val prefs = app.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            if (!prefs.getBoolean(K_ADOPTED, false)) {
                val old = app.getSharedPreferences(LEGACY_PREFS, Context.MODE_PRIVATE).all
                prefs.edit().apply {
                    old.forEach { (id, json) -> if (json is String) putString(id, json) }
                    putBoolean(K_ADOPTED, true)
                }.commit()
            }
            return prefs
        }
    }
}

/**
 * Chip colours a preset can wear. Chosen to stay legible on a dark surface and to be
 * distinguishable from each other at the size they are actually used — a 6dp dot on a chip and a
 * tint on a pinned card — and held at one saturation and lightness so no single swatch shouts
 * over its neighbours. Deliberately NOT the presence green ([HostCard]'s online dot), which means
 * something else entirely.
 *
 * **Ordered by hue**, so the picker reads as one sweep of the colour wheel rather than a bag of
 * colours; the degrees are in the comments to keep it that way when one is swapped out. That order
 * is also the order [nextAccent] hands them out in, so a user creating presets one after another
 * walks the spectrum instead of getting an arbitrary sequence.
 */
val PRESET_ACCENTS = listOf(
    "#FF8A4C", // orange   21°
    "#FBBF24", // amber    45°
    "#A3E635", // lime     82°
    "#34D399", // green   160°
    "#22D3EE", // cyan    187°
    "#60A5FA", // blue    213°
    "#818CF8", // indigo  239°
    "#A78BFA", // violet  258°
    "#F472B6", // pink    330°
    "#FB7185", // rose    350°
)

/** The first accent no existing preset is using, so two presets don't look alike by accident. */
fun nextAccent(existing: List<StreamPreset>): String {
    val taken = existing.mapNotNull { it.accent?.lowercase() }.toSet()
    return PRESET_ACCENTS.firstOrNull { it.lowercase() !in taken } ?: PRESET_ACCENTS.first()
}

/**
 * A new, empty preset: it inherits everything, which is the right creation default under
 * inherit-by-exception (Duplicate covers "start from that other preset"). The id is 12 lowercase
 * hex characters — the shape the Rust `new_preset_id` mints.
 *
 * [accent] is presentation, not a setting, so it does NOT inherit — a preset with no colour would
 * be indistinguishable from the defaults everywhere the accent is the whole signal (a bound card's
 * chip, a pinned card's tint). Callers creating a preset from the UI pass [nextAccent].
 */
fun newPreset(name: String, accent: String? = null): StreamPreset =
    StreamPreset(id = newPresetId(), name = name, accent = accent)

private val PRESET_ID_RNG = SecureRandom()

fun newPresetId(): String {
    val b = ByteArray(6)
    PRESET_ID_RNG.nextBytes(b)
    return b.joinToString("") { "%02x".format(it) }
}

/**
 * The settings a connect to [host] should use: the resolved preset's overrides on top of these
 * globals, resolved ONCE per connect (matching the latch-at-connect model the "applies from the
 * next session" footers promise). See [PresetStore.resolveFor] for the precedence.
 */
fun Settings.effectiveFor(preset: StreamPreset?): Settings =
    preset?.overrides?.apply(this) ?: this

// ---- org.json null-vs-absent helpers (optInt and friends can't tell 0 from "not there") ---------


private fun JSONObject.optStringOrNull(key: String): String? =
    if (has(key)) optString(key).ifEmpty { null } else null
