package io.unom.punktfunk.console

import android.os.Vibrator
import android.view.InputDevice
import io.unom.punktfunk.HostActions
import io.unom.punktfunk.ProfilesAnswer
import io.unom.punktfunk.Settings
import io.unom.punktfunk.SettingsFields
import io.unom.punktfunk.SpeedTestPhase
import io.unom.punktfunk.StatsVerbosity
import io.unom.punktfunk.StreamPreset
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.library.DEFAULT_MGMT_PORT
import io.unom.punktfunk.kit.library.Download
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.RunningGame
import io.unom.punktfunk.kit.library.TitleInstall
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.deviceDetail
import io.unom.punktfunk.matches
import io.unom.punktfunk.padInfoOf
import org.json.JSONArray
import org.json.JSONObject

/**
 * The JSON that crosses into the Skia console — written in the console's OWN model shapes
 * (`crates/pf-console-ui/src/model.rs` `HostRow`/`WakeStatus`/`PairPhase`, `library.rs`
 * `LibraryGame`/`LibraryPhase`, `pf-client-core/src/trust.rs` `Settings`/`KnownHosts`), so there
 * is no Android-side mirror type to drift; the Rust structs deserialize these directly.
 * `clients/shared/console-bridge-vectors.json` holds a sample of each pushed model these write.
 */
internal object ConsoleJson {
    // ---- host rows (`HostRow`) ------------------------------------------------------------

    /** `HostRow.key` — the pinned fingerprint when there is one, else `addr:port` (Rust parity). */
    fun rowKey(fpHex: String, address: String, port: Int): String =
        if (fpHex.isEmpty()) "$address:$port" else fpHex

    /**
     * A pinned card's `HostRow.key`: the host's [rowKey], a NUL, then the preset id. Pinned by
     * `pinned_key` in `clients/shared/console-vectors.json`, which the console splits back.
     */
    fun pinnedKey(key: String, presetId: String): String = "$key\u0000$presetId"

    /** The host half of a row key: a pinned card's key without its preset id. */
    fun hostKey(key: String): String = key.substringBefore('\u0000')

    private fun presetChip(p: StreamPreset): JSONObject = JSONObject()
        .put("id", p.id)
        .put("name", p.name)
        .put("accent", p.accent ?: JSONObject.NULL)
        // Read by the speed test alone: a preset that PINS bitrate is the layer its host
        // streams at, so the console must not offer to write the global default instead.
        .put("bitrate_kbps", p.overrides.bitrateKbps ?: JSONObject.NULL)

    /** A host's advertised actions in the console model's shape (`HostRow.actions`). */
    private fun actionRows(actions: List<HostActions.Action>?): JSONArray {
        val arr = JSONArray()
        for (a in actions.orEmpty()) {
            arr.put(
                JSONObject()
                    .put("id", a.id)
                    .put("label", a.label)
                    .put("danger", a.danger)
                    .put("available", a.available)
                    .put("unavailable_reason", a.unavailableReason),
            )
        }
        return arr
    }

    /**
     * The home carousel: saved hosts in the order they were added, each followed by its pinned
     * preset cards, then discovered-but-unsaved hosts by name. The console applies the player's
     * sort on top. The desktop and Apple producers send the same rows;
     * `clients/shared/host-row-vectors.json` holds all three to it.
     */
    fun hostRows(
        saved: List<KnownHost>,
        discovered: List<DiscoveredHost>,
        reachable: Set<String>,
        presets: List<StreamPreset>,
        /** What each paired host last said this device may do TO it, by fingerprint
         *  (`design/host-actions.md` §7). Absent = no rows, which is also what an older host
         *  and an ungranted device produce. */
        hostActions: Map<String, List<HostActions.Action>> = emptyMap(),
        /** What each paired host has up, by fingerprint. Absent = nothing, or nobody has
         *  asked yet; the tile draws no line either way. */
        running: Map<String, String> = emptyMap(),
    ): String {
        val out = JSONArray()
        // The store keeps no insertion order; an undated record predates the stamp, so it
        // goes first and keeps the order it came in.
        for (h in saved.sortedWith(compareBy(nullsFirst<Long>()) { it.addedAt })) {
            val key = rowKey(h.fpHex, h.address, h.port)
            val advert = discovered.firstOrNull { h.matches(it) }
            // Presence is the probe alone, by record id. An advert only says where to look: a
            // suspending host sends no mDNS goodbye, so its record lingers for up to 75 minutes —
            // long enough to keep the pip green and, since `can_wake` reads `!online`, the Wake
            // row hidden.
            val online = h.id in reachable
            val base = JSONObject()
                .put("key", key)
                .put("id", h.id)
                .put("name", h.name.ifBlank { h.address })
                .put("addr", h.address)
                .put("port", h.port)
                .put("fp_hex", h.fpHex)
                .put("paired", h.paired)
                .put("saved", true)
                .put("online", online)
                .put("mgmt_port", advert?.mgmtPort ?: h.mgmtPort ?: DEFAULT_MGMT_PORT)
                .put("can_wake", !online && h.mac.isNotEmpty())
                .put("clipboard_sync", h.clipboardSync)
                .put("last_used", h.lastUsed ?: JSONObject.NULL)
                .put("os", advert?.os?.takeIf { it.isNotEmpty() } ?: h.os)
                .put("actions", actionRows(hostActions[h.fpHex]))
                .put("pin", JSONObject.NULL)
                .put(
                    "bound_preset",
                    h.presetId?.let { id -> presets.firstOrNull { it.id == id } }
                        ?.let(::presetChip) ?: JSONObject.NULL,
                )
                .put("running", running[h.fpHex].orEmpty())
                // Ids, not chips: the bind screen only compares them. Pinned copies below
                // inherit the map — a card is the same host's shelf.
                .put("game_presets", JSONObject(h.gamePresets))
                .put("profile", profileJson(h))
            out.put(base)
            // A pinned card shares the primary tile's live state under its own key.
            for (pid in h.pinnedPresetIds.distinct()) {
                val p = presets.firstOrNull { it.id == pid } ?: continue
                out.put(
                    JSONObject(base.toString())
                        .put("key", pinnedKey(key, p.id))
                        .put("pin", presetChip(p))
                        .put("bound_preset", JSONObject.NULL),
                )
            }
        }
        val extra = discovered.filter { d -> saved.none { it.matches(d) } }
            .sortedBy { it.name.lowercase() }
        for (d in extra) {
            val fp = d.fingerprint.orEmpty()
            out.put(
                JSONObject()
                    .put("key", rowKey(fp, d.host, d.port))
                    .put("name", d.name.ifBlank { d.host })
                    .put("addr", d.host)
                    .put("port", d.port)
                    .put("fp_hex", fp)
                    .put("paired", false)
                    .put("saved", false)
                    .put("online", true)
                    .put("mgmt_port", d.mgmtPort ?: DEFAULT_MGMT_PORT)
                    .put("can_wake", false)
                    .put("clipboard_sync", false)
                    .put("last_used", JSONObject.NULL)
                    .put("os", d.os)
                    .put("pin", JSONObject.NULL)
                    .put("bound_preset", JSONObject.NULL)
                    // Unsaved: no identity to ask what it is running with.
                    .put("running", ""),
            )
        }
        return out.toString()
    }

    /** One `HostRow` for a console entry (`{"library": <HostRow>}`) — the shelf to open. */
    fun hostRow(h: KnownHost, pin: StreamPreset?, presets: List<StreamPreset>): JSONObject {
        val key = rowKey(h.fpHex, h.address, h.port)
        return JSONObject()
            .put("key", if (pin == null) key else pinnedKey(key, pin.id))
            .put("id", h.id)
                .put("name", h.name.ifBlank { h.address })
            .put("addr", h.address)
            .put("port", h.port)
            .put("fp_hex", h.fpHex)
            .put("paired", h.paired)
            .put("saved", true)
            .put("online", true)
            .put("mgmt_port", h.mgmtPort ?: DEFAULT_MGMT_PORT)
            .put("can_wake", false)
            .put("clipboard_sync", h.clipboardSync)
            .put("last_used", h.lastUsed ?: JSONObject.NULL)
            .put("os", h.os)
            .put("pin", pin?.let(::presetChip) ?: JSONObject.NULL)
            .put(
                "bound_preset",
                if (pin != null) JSONObject.NULL
                else h.presetId?.let { id -> presets.firstOrNull { it.id == id } }
                    ?.let(::presetChip) ?: JSONObject.NULL,
            )
            .put("game_presets", JSONObject(h.gamePresets))
            .put("profile", profileJson(h))
    }

    /** One `enumerate` row, as the console's `ListedProfile` reads it. */
    fun profileRow(p: io.unom.punktfunk.kit.ListedProfile): JSONObject = JSONObject()
        .put("id", p.id)
        .put("display_name", p.displayName)
        .put("accent", p.accent ?: JSONObject.NULL)
        .put("owner", p.owner)
        .put("legacy_seat", p.legacySeat)
        .put(
            "seat",
            p.seat?.let {
                JSONObject()
                    .put("state", it.state.name.lowercase())
                    .put("detail", it.detail ?: JSONObject.NULL)
                    .put("occupant", it.occupant ?: JSONObject.NULL)
                    .put("steam_sign_in", it.steamSignIn ?: JSONObject.NULL)
            } ?: JSONObject.NULL,
        )

    /** The saved profile pick as `{id, display_name}`, or `null`. */
    private fun profileJson(h: KnownHost): Any = h.asProfile
        ?.let { JSONObject().put("id", it.id).put("display_name", it.displayName) }
        ?: JSONObject.NULL

    /** `KnownHosts` (Rust) — only what the console needs to build a link: id, address, fp. */
    fun knownHosts(saved: List<KnownHost>): String {
        val hosts = JSONArray()
        for (h in saved) {
            hosts.put(
                JSONObject()
                    .put("name", h.name)
                    .put("addr", h.address)
                    .put("port", h.port)
                    .put("fp_hex", h.fpHex)
                    .put("paired", h.paired)
                    .put("id", h.id)
                    .put("mac", JSONArray(h.mac))
                    .put("os", h.os)
                    .put("mgmt_port", h.mgmtPort ?: JSONObject.NULL)
                    .put("preset_id", h.presetId ?: JSONObject.NULL)
                    .put("pinned_presets", JSONArray(h.pinnedPresetIds)),
            )
        }
        return JSONObject().put("hosts", hosts).toString()
    }

    /** The catalog with each preset's overrides, so a settings row can say when a host's
     *  bound preset outranks the global it shows. */
    fun presets(presets: List<StreamPreset>): String {
        val out = JSONArray()
        for (p in presets) {
            out.put(
                JSONObject()
                    .put("id", p.id)
                    .put("name", p.name)
                    .put("overrides", p.overrides.toConsoleJson()),
            )
        }
        return out.toString()
    }

    // ---- wake / pair ------------------------------------------------------------------------

    fun wakeStatus(
        key: String,
        name: String,
        seconds: Int,
        timedOut: Boolean,
        online: Boolean,
        thenConnect: Boolean,
    ): String = JSONObject()
        .put("key", key)
        .put("name", name)
        .put("seconds", seconds)
        .put("timed_out", timedOut)
        .put("online", online)
        .put("then_connect", thenConnect)
        .toString()

    fun pairIdle(): String = "\"Idle\""
    fun pairBusy(): String = "\"Busy\""
    fun pairFailed(msg: String): String = JSONObject().put("Failed", msg).toString()
    fun pairPaired(key: String): String =
        JSONObject().put("Paired", JSONObject().put("key", key)).toString()

    // ---- speed test / profiles ----------------------------------------------------------------

    /** `SpeedPhase`: unit variants are bare strings. */
    fun speedPhase(p: SpeedTestPhase): String = when (p) {
        SpeedTestPhase.Connecting -> "\"Connecting\""
        SpeedTestPhase.Measuring -> "\"Measuring\""
        is SpeedTestPhase.Failed -> JSONObject().put("Failed", p.message).toString()
        is SpeedTestPhase.Done -> JSONObject().put(
            "Done",
            JSONObject()
                .put("throughput_kbps", p.throughputKbps)
                .put("wall", p.wall)
                .put(
                    "clean",
                    p.clean?.let {
                        JSONObject()
                            .put("rate_kbps", it.rateKbps)
                            .put("loss_pct", it.lossPct)
                            .put("jitter_us", it.jitterUs)
                    } ?: JSONObject.NULL,
                )
                .put("recommended_kbps", p.recommendedKbps)
                .put(
                    "findings",
                    JSONArray(
                        p.findings.map { f ->
                            JSONObject().put("id", f.id).put("severity", f.severity)
                                .put("numbers", JSONArray(f.numbers))
                        },
                    ),
                ),
        ).toString()
    }

    /** `SpeedPhase::Progress`: a mid-burst figure for the console's graph. */
    fun speedProgress(kbps: Int): String =
        JSONObject().put("Progress", JSONObject().put("kbps", kbps)).toString()

    /** The answer to `ConsoleCmd::FetchProfiles`, as the console's `ProfilesAnswer`. */
    fun profilesAnswer(answer: ProfilesAnswer): String = when (answer) {
        is ProfilesAnswer.Listed -> JSONObject().put("Listed", JSONArray(answer.rows.map(::profileRow))).toString()
        ProfilesAnswer.NoProfiles -> "\"NoProfiles\""
        is ProfilesAnswer.Failed -> JSONObject().put("Failed", answer.why).toString()
    }

    // ---- library ------------------------------------------------------------------------------

    /**
     * `[LibraryGame]` from the Kotlin catalog — the desktop service's `to_model` mapping.
     * Without `stats` the Recent and Most played sorts fall back to host order.
     */
    fun libraryGames(games: List<GameEntry>): String {
        val out = JSONArray()
        for (g in games) {
            out.put(
                JSONObject()
                    .put("id", g.id)
                    .put("title", g.title)
                    .put("store", g.store)
                    .put("launcher", g.isLauncher)
                    .put("icon", g.icon?.takeIf(::validIconToken) ?: "")
                    .put("platform", g.platform ?: JSONObject.NULL)
                    .put("developer", g.developer ?: JSONObject.NULL)
                    .put("year", g.releaseYear ?: JSONObject.NULL)
                    .put("genres", JSONArray(g.genres))
                    .put("stats", g.stats?.toJson() ?: JSONObject.NULL)
                    .put("running", false)
                    .put("install", g.install?.let(::installJson) ?: JSONObject.NULL),
            )
        }
        return out.toString()
    }

    private fun installJson(i: TitleInstall): JSONObject = JSONObject().put("state", i.state).apply {
        i.sizeBytes?.let { put("size_bytes", it) }
        i.freeBytes?.let { put("free_bytes", it) }
    }

    /** `GameEntry::icon_token`'s re-validation: lowercase-first, ≤ 32 chars of [a-z0-9-]. */
    private fun validIconToken(t: String): Boolean =
        t.isNotEmpty() && t.length <= 32 && t[0] in 'a'..'z' &&
            t.all { it in 'a'..'z' || it in '0'..'9' || it == '-' }

    fun libraryError(title: String, body: String, canRetry: Boolean): String = JSONObject()
        .put(
            "Error",
            JSONObject().put("title", title).put("body", body).put("can_retry", canRetry),
        )
        .toString()

    fun stringArray(items: Collection<String>): String = JSONArray(items).toString()

    /** `/status` `downloads[]` and `grants`, as the console's `DownloadsPush`. */
    fun downloads(downloads: List<Download>, grants: Int?): String {
        val out = JSONArray()
        for (d in downloads) {
            val o = JSONObject().put("app_id", d.appId).put("state", d.state)
                .put("done_bytes", d.doneBytes)
            d.totalBytes?.let { o.put("total_bytes", it) }
            d.rateBps?.let { o.put("rate_bps", it) }
            d.etaS?.let { o.put("eta_s", it) }
            d.phase?.let { o.put("phase", it) }
            d.error?.let { o.put("error", it) }
            out.put(o)
        }
        return JSONObject().put("downloads", out).put("grants", grants ?: JSONObject.NULL).toString()
    }

    /** `/status` games as the console's `RunningGame` mirror; an entry without an id has no tile. */
    fun runningGames(games: List<RunningGame>): String {
        val out = JSONArray()
        for (g in games) {
            val id = g.appId ?: continue
            out.put(
                JSONObject().put("app_id", id).put("state", g.state)
                    .put("awaiting_window", g.awaitingWindow).put("endable", g.endable),
            )
        }
        return out.toString()
    }

    // ---- pads -------------------------------------------------------------------------------

    /**
     * A pad the console must list that owns no [InputDevice]: a captured Steam Controller 2,
     * whose claim detaches the kernel node, or a USB one still waiting on its grant.
     */
    data class ExtraPad(
        val name: String,
        val key: String,
        val pref: Int,
        val detail: String,
        val forwarded: Boolean,
        /** Its own transport can buzz it — without this the console's row reads "No rumble". */
        val rumble: Boolean,
    )

    /**
     * `{"label", "pref", "pads": [...]}` — the controller chip's text (the driving pad's name),
     * the glyph style's pref byte, and one entry per connected pad for the settings rows and the
     * console's Connected-controllers screen. [extras] are appended, and the first one names the
     * chip when no `InputDevice` drives it. [body] is this device's own vibrator.
     *
     * `detail`/`forwarded`/`rumble` come straight from [padInfoOf], the same reader the touch
     * Controllers screen renders from: the support answer a user gets must not depend on which
     * interface asked, and two readers of `InputDevice` would be two answers waiting to drift.
     */
    fun pads(
        pads: List<InputDevice>,
        driving: InputDevice?,
        extras: List<ExtraPad> = emptyList(),
        body: Vibrator? = null,
        others: List<InputDevice> = emptyList(),
    ): String {
        val arr = JSONArray()
        for (d in pads) {
            val info = padInfoOf(d, body)
            val entry = JSONObject()
                .put("name", d.name)
                .put("key", "${d.vendorId}:${d.productId}:${d.name}")
                .put("pref", Gamepad.prefFor(d))
                .put("steam_virtual", false)
                .put("detail", info.detail)
                .put("forwarded", info.forwarded)
                .put("rumble", info.canRumble)
            val battery = if (android.os.Build.VERSION.SDK_INT >= 31) {
                val b = d.batteryState
                if (b.isPresent && b.capacity >= 0f) {
                    JSONObject()
                        .put("percent", (b.capacity * 100f).toInt().coerceIn(0, 100))
                        .put(
                            "charging",
                            b.status == android.os.BatteryManager.BATTERY_STATUS_CHARGING ||
                                b.status == android.os.BatteryManager.BATTERY_STATUS_FULL,
                        )
                } else null
            } else null
            entry.put("battery", battery ?: JSONObject.NULL)
            arr.put(entry)
        }
        for (e in extras) {
            arr.put(
                JSONObject()
                    .put("name", e.name)
                    .put("key", e.key)
                    .put("pref", e.pref)
                    .put("steam_virtual", false)
                    .put("detail", e.detail)
                    .put("forwarded", e.forwarded)
                    .put("rumble", e.rumble)
                    .put("battery", JSONObject.NULL),
            )
        }
        val extra = extras.firstOrNull()
        val otherRows = JSONArray()
        for (d in others) {
            otherRows.put(JSONObject().put("name", d.name).put("kind", kindOf(d)).put("detail", deviceDetail(d)))
        }
        return JSONObject()
            .put("label", driving?.name ?: extra?.name ?: JSONObject.NULL)
            .put("pref", driving?.let { Gamepad.prefFor(it) } ?: extra?.pref ?: JSONObject.NULL)
            .put("pads", arr)
            .put("others", otherRows)
            .toString()
    }

    /**
     * Real, plugged-in input devices that are not controllers — a pad Android misreads lands
     * here, so it is listed somewhere. Below API 29 there is no `isExternal`; all are listed.
     */
    fun otherInputs(): List<InputDevice> = InputDevice.getDeviceIds().toList()
        .mapNotNull { InputDevice.getDevice(it) }
        .filter { !it.isVirtual && !Gamepad.looksLikeController(it) }
        .filter { android.os.Build.VERSION.SDK_INT < 29 || it.isExternal }

    private fun kindOf(d: InputDevice): String = when {
        d.supportsSource(InputDevice.SOURCE_MOUSE) -> "mouse"
        d.keyboardType == InputDevice.KEYBOARD_TYPE_ALPHABETIC -> "keyboard"
        d.supportsSource(InputDevice.SOURCE_DPAD) -> "remote"
        else -> "other"
    }

    // ---- settings (`trust::Settings`) -------------------------------------------------------

    /**
     * The console's settings document: [base] is the last snapshot the console saved (it owns
     * keys Android has no field for — `library_sort`, `library_view`, `reduce_motion`, …), and
     * every field Android DOES own is written over it from [s], so the touch UI's edits win.
     * `trust::Settings` is `#[serde(default)]`, so a partial document is fine.
     */
    fun settings(s: Settings, base: JSONObject?): JSONObject {
        val j = base?.let { JSONObject(it.toString()) } ?: JSONObject()
        // Every row, both shells' keys included: carrying `start_in`/`default_host` in `base`
        // alone would work until the touch UI wrote one, at which point the next push would
        // paste the console's older copy back over it. The `android.*` rows ride
        // `Settings::extra`, which is `#[serde(flatten)]` — TOP-LEVEL keys, never nested.
        SettingsFields.ALL.forEach { it.consoleWrite(j, s) }
        j.put("show_stats", s.statsVerbosity != StatsVerbosity.OFF)
        // A store written by the nesting build carries the stale wrapper; drop it rather than
        // round-trip a copy of these keys that nothing reads for the life of the install.
        j.remove("extra")
        return j
    }

    /**
     * The console saved [j]: fold every key Android owns back into [s]. Unknown values snap to
     * the field's current value — a newer console's spelling must never corrupt the store.
     */
    fun applySettings(s: Settings, j: JSONObject): Settings =
        SettingsFields.ALL.fold(s) { acc, f -> f.consoleRead(j, acc) }
}
