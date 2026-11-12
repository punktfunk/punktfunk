package io.unom.punktfunk

import android.content.res.Resources
import androidx.annotation.StringRes
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.library.MgmtReply
import io.unom.punktfunk.kit.library.mgmtCall
import io.unom.punktfunk.kit.say
import io.unom.punktfunk.kit.security.ClientIdentity
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject

/**
 * Which host screen this device streams, and how: the host's answer
 * (`GET /api/v1/display/targets`), the Screen section drawn from it ([Targets.rows]) and the
 * write of this device's pick ([setPick]). The Kotlin port of `pf_client_core::display_targets`,
 * on the same mTLS lane as [HostActions]; `clients/shared/screen-rows-vectors.json` pins both.
 *
 * The host names the monitors; this app words the roles. The pick is this device's row on the
 * host, so nothing here is stored. Blocking — call it off the main thread.
 */
object DisplayTargets {
    /** The id the host gives the own-screen row, and the JSON of [DisplayTarget.Own]. */
    private const val OWN = "own"

    /** What a session streams: this device's own virtual screen, or a host monitor by connector. */
    sealed interface DisplayTarget {
        data object Own : DisplayTarget

        data class Monitor(val connector: String) : DisplayTarget

        /** `"own"` or `{"monitor": "HDMI-A-1"}`, the host's spelling. */
        fun json(): Any = when (this) {
            Own -> OWN
            is Monitor -> JSONObject().put("monitor", connector)
        }

        companion object {
            /** [json] read back; null for anything else. */
            fun of(v: Any?): DisplayTarget? = when {
                v == OWN -> Own
                v is JSONObject && !v.isNull("monitor") -> Monitor(v.optString("monitor"))
                else -> null
            }
        }
    }

    /**
     * How this device uses the host's screens when it streams its own: the host's monitors off,
     * on with apps opening here, or on with this one more screen beside them. Declared in the
     * Screen section's order.
     */
    enum class Role(
        /** The host's word, which is also the row's id. */
        val id: String,
        /** The `topology` a pick of this role writes. */
        val topology: String,
        @StringRes val label: Int,
        @StringRes val hint: Int,
    ) {
        ONLY("only", "exclusive", R.string.screen_role_only, R.string.screen_role_only_hint),
        MAIN("main", "primary", R.string.screen_role_main, R.string.screen_role_main_hint),
        SECOND("second", "extend", R.string.screen_role_second, R.string.screen_role_second_hint),
        ;

        companion object {
            /** A host's word; null for one this build doesn't know. */
            fun of(v: Any?): Role? = entries.firstOrNull { it.id == v }
        }
    }

    /** The answer's `role`: present only where this device's own screen can take a role. */
    data class RoleInfo(
        /** The role of this device's own pick; null follows [hostDefault]. */
        val yours: Role? = null,
        val hostDefault: Role = Role.ONLY,
        /** The roles this host can give, in any order. */
        val offered: List<Role> = emptyList(),
    ) {
        /** The role a connect on the own screen gets now. */
        fun current(): Role = yours ?: hostDefault

        fun json(): JSONObject = JSONObject()
            .put("yours", yours?.id ?: JSONObject.NULL)
            .put("host_default", hostDefault.id)
            .put("offered", JSONArray(offered.map { it.id }))
    }

    /** The body of `PUT /display/clients/{fp}`: the screen, and for a role its topology. */
    data class Pick(val screen: DisplayTarget, val topology: String? = null) {
        fun json(): JSONObject = JSONObject().put("screen", screen.json()).apply {
            topology?.let { put("topology", it) }
        }

        companion object {
            /** [json] read back, or the console's `SetScreen` command, which carries the same keys. */
            fun of(o: JSONObject): Pick? = DisplayTarget.of(o.opt("screen"))?.let { Pick(it, o.str("topology")) }
        }
    }

    /** One row of the picker, as the host sends it. */
    data class Target(
        /** `own`, else the connector. */
        val id: String,
        /** The host's words: a monitor's description, or its English for the own row. */
        val label: String = "",
        val connector: String? = null,
        /** `3840x2160@60`, as the host sends it. */
        val mode: String? = null,
        val primary: Boolean = false,
        /** False when the compositor reports the head off. */
        val enabled: Boolean = true,
        /** The pairing name of the device streaming this head now. */
        val inUseBy: String? = null,
    ) {
        /** What picking this row writes. */
        fun target(): DisplayTarget = if (id == OWN) DisplayTarget.Own else DisplayTarget.Monitor(id)

        /** The row's word: this app's wording for the own row, the host's label for a monitor. */
        fun name(res: Resources): String = when {
            id == OWN -> res.getString(R.string.screen_own)
            label.isEmpty() -> id
            else -> label
        }

        /** Connector and mode, under the row; the connector drops out when it is the label. */
        fun detail(): String = listOfNotNull(connector?.takeIf { it != label }, mode).joinToString(" · ")

        /** Why the row can't be picked now, shown under it; null for a pickable row. */
        fun reason(res: Resources): String? = when {
            inUseBy != null -> res.getString(R.string.screen_in_use, inUseBy)
            !enabled -> res.getString(R.string.screen_off)
            else -> null
        }

        /** This target as a Screen row, checked when it is [current]. */
        fun row(current: DisplayTarget, res: Resources): ScreenRow {
            val why = reason(res)
            return ScreenRow(
                kind = if (id == OWN) RowKind.OWN else RowKind.MONITOR,
                id = id,
                label = name(res),
                detail = detail(),
                checked = target() == current,
                pickable = why == null,
                reason = why,
                pick = Pick(target()),
            )
        }
    }

    /** What a row of the Screen section stands for; [RowKind.OWN] is an older host's own row. */
    enum class RowKind { ROLE, MONITOR, OWN }

    /**
     * One row of the Screen section, worded. A shell draws [label], and [reason], else [hint],
     * else [detail] under it. [header], when set, heads a group from this row on.
     */
    data class ScreenRow(
        val kind: RowKind,
        /** `only` · `main` · `second`, a connector, or `own`: stable across refreshes. */
        val id: String,
        val label: String,
        val hint: String? = null,
        /** A monitor's connector and mode; empty for the rest. */
        val detail: String = "",
        val header: String? = null,
        /** What a connect gets now. */
        val checked: Boolean,
        /** False draws the row greyed: a head off or taken, or the host decides. */
        val pickable: Boolean = true,
        /** Why a monitor can't be streamed now. */
        val reason: String? = null,
        /** What picking the row writes; null when it is checked or not pickable. */
        val pick: Pick?,
    )

    /** The host's answer for this device. */
    data class Targets(
        /** The host's settings file pins the screen: the rows are not offered. */
        val locked: Boolean = false,
        /** This device may pick. A host that omits it predates the grant and lets every device. */
        val mayChoose: Boolean = true,
        val hostDefault: DisplayTarget = DisplayTarget.Own,
        /** This device's own row; null follows [hostDefault]. */
        val yours: DisplayTarget? = null,
        val targets: List<Target> = emptyList(),
        val role: RoleInfo? = null,
    ) {
        /** The pick a connect gets now: this device's row, else the host's default. */
        fun current(): DisplayTarget = yours ?: hostDefault

        /** Why nothing in [rows] can be picked: the settings file, else the missing grant. */
        fun note(res: Resources): String? = when {
            locked -> res.getString(R.string.screen_locked_hint)
            !mayChoose -> res.getString(R.string.screen_host_decides)
            else -> null
        }

        /**
         * The Screen section. With [role]: the offered roles as only, main, second, then the
         * monitors under "Show a monitor", the own row left out. Without: one row per target in
         * the host's order. Checked is the monitor this device streams, else its role. Under a
         * [note] only the checked row stays, greyed.
         */
        fun rows(res: Resources): List<ScreenRow> {
            val current = current()
            val all = role?.let { info ->
                val own = current == DisplayTarget.Own
                val roles = Role.entries.filter { it in info.offered }.map { r ->
                    ScreenRow(
                        kind = RowKind.ROLE,
                        id = r.id,
                        label = res.getString(r.label),
                        hint = res.getString(r.hint),
                        checked = own && r == info.current(),
                        pick = Pick(DisplayTarget.Own, r.topology),
                    )
                }
                val monitors = targets.filter { it.id != OWN }.mapIndexed { i, t ->
                    t.row(current, res).copy(header = if (i == 0) res.getString(R.string.screen_show_monitor) else null)
                }
                roles + monitors
            } ?: targets.map { it.row(current, res) }
            val decides = note(res) != null
            return all.filter { !decides || it.checked }.map { r ->
                val pickable = r.pickable && !decides
                r.copy(
                    pickable = pickable,
                    header = r.header.takeIf { !decides },
                    pick = r.pick.takeIf { pickable && !r.checked },
                )
            }
        }

        /**
         * The card's line: "Streams LG TV" for a monitor, the role's name for a main or second
         * screen. Null for the only screen and an older host's own screen: the default is silent.
         */
        fun cardLine(res: Resources): String? = when (val c = current()) {
            is DisplayTarget.Monitor -> res.getString(
                R.string.screen_streams,
                targets.firstOrNull { it.id == c.connector }?.name(res) ?: c.connector,
            )
            DisplayTarget.Own -> role?.current()?.takeIf { it != Role.ONLY }?.let { res.getString(it.label) }
        }

        /** The console's `HostRow.screens`: the host's own shape, which the Rust model reads. */
        fun json(): JSONObject = JSONObject()
            .put("locked", locked)
            .put("may_choose", mayChoose)
            .put("host_default", hostDefault.json())
            .put("yours", yours?.json() ?: JSONObject.NULL)
            .put(
                "targets",
                JSONArray(
                    targets.map { t ->
                        JSONObject()
                            .put("id", t.id)
                            .put("label", t.label)
                            .put("connector", t.connector ?: JSONObject.NULL)
                            .put("mode", t.mode ?: JSONObject.NULL)
                            .put("primary", t.primary)
                            .put("enabled", t.enabled)
                            .put("in_use_by", t.inUseBy ?: JSONObject.NULL)
                    },
                ),
            )
            .put("role", role?.json() ?: JSONObject.NULL)
    }

    private fun JSONObject.str(key: String): String? = if (isNull(key)) null else optString(key)

    /**
     * A `GET /display/targets` body; null when it is not one. Every field defaults, like the Rust
     * model's, and `role` never fails it: an unknown word drops out of `offered`, reads as null in
     * `yours` and as [Role.ONLY] in `host_default`; a `role` that is not an object is none.
     */
    fun parse(body: String): Targets? = runCatching {
        val o = JSONObject(body)
        val rows = o.optJSONArray("targets") ?: JSONArray()
        Targets(
            locked = o.optBoolean("locked"),
            mayChoose = o.optBoolean("may_choose", true),
            hostDefault = DisplayTarget.of(o.opt("host_default")) ?: DisplayTarget.Own,
            yours = DisplayTarget.of(o.opt("yours")),
            targets = (0 until rows.length()).mapNotNull { i ->
                val t = rows.optJSONObject(i) ?: return@mapNotNull null
                Target(
                    id = t.optString("id"),
                    label = t.optString("label"),
                    connector = t.str("connector"),
                    mode = t.str("mode"),
                    primary = t.optBoolean("primary"),
                    enabled = t.optBoolean("enabled", true),
                    inUseBy = t.str("in_use_by"),
                )
            },
            role = o.optJSONObject("role")?.let { r ->
                val offered = r.optJSONArray("offered") ?: JSONArray()
                RoleInfo(
                    yours = Role.of(r.opt("yours")),
                    hostDefault = Role.of(r.opt("host_default")) ?: Role.ONLY,
                    offered = (0 until offered.length()).mapNotNull { Role.of(offered.opt(it)) },
                )
            },
        )
    }.getOrNull()

    /**
     * `GET /api/v1/display/targets`. Null on any miss, like [HostActions.list]: a host without
     * the route has no Screen section.
     */
    fun fetch(identity: ClientIdentity, addr: String, mgmtPort: Int, fpHex: String, streamPort: Int = 0): Targets? {
        val reply = mgmtCall(identity, addr, mgmtPort, fpHex, streamPort = streamPort) { base ->
            Request.Builder().url("$base/api/v1/display/targets").get().build()
        }
        if (reply !is MgmtReply.Answer || !reply.ok) return null
        return parse(reply.body)
    }

    /**
     * `PUT /api/v1/display/clients/{this device's fingerprint}` with a row's [Pick]. Null when the
     * host took it, else the notice; [label] is the row's word. The caller fetches again.
     */
    fun setPick(
        identity: ClientIdentity,
        addr: String,
        mgmtPort: Int,
        fpHex: String,
        pick: Pick,
        label: String,
        streamPort: Int = 0,
    ): Say? {
        val device = certFingerprint(identity.certPem)
            ?: return say(R.string.hostcard_action_failed, label, MgmtReply.Unreachable.why)
        val hex = device.joinToString("") { "%02x".format(it) }
        val body = pick.json().toString()
        val reply = mgmtCall(identity, addr, mgmtPort, fpHex, streamPort = streamPort) { base ->
            Request.Builder()
                .url("$base/api/v1/display/clients/$hex")
                .put(body.toRequestBody("application/json".toMediaType()))
                .build()
        }
        return if (reply is MgmtReply.Answer && reply.ok) null else say(R.string.hostcard_action_failed, label, reply.why)
    }
}
