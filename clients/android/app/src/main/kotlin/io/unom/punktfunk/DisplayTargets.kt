package io.unom.punktfunk

import android.content.res.Resources
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
 * Which host screen this device streams (`design/screen-pick.md`): the rows the host offers it
 * (`GET /api/v1/display/targets`) and the write of its own pick. The Kotlin port of
 * `pf_client_core::display_targets`, on the same mTLS lane as [HostActions].
 *
 * The host resolves every row for the caller; this renders them and never builds a label. The
 * pick is this device's row on the host, so nothing here is stored. Blocking — call it off the
 * main thread.
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
    }

    /** The host's answer for this device. */
    data class Targets(
        /** The host's settings file pins the screen: the rows are not offered. */
        val locked: Boolean = false,
        val hostDefault: DisplayTarget = DisplayTarget.Own,
        /** This device's own row; null follows [hostDefault]. */
        val yours: DisplayTarget? = null,
        val targets: List<Target> = emptyList(),
    ) {
        /** The pick a connect gets now: this device's row, else the host's default. */
        fun current(): DisplayTarget = yours ?: hostDefault

        /** The card's line: "Streams LG TV" for a monitor, the own row's word for its own screen. */
        fun summary(res: Resources): String = when (val c = current()) {
            DisplayTarget.Own -> res.getString(R.string.screen_own)
            is DisplayTarget.Monitor -> res.getString(
                R.string.screen_streams,
                targets.firstOrNull { it.id == c.connector }?.name(res) ?: c.connector,
            )
        }

        /** The console's `HostRow.screens`: the host's own shape, which the Rust model reads. */
        fun json(): JSONObject = JSONObject()
            .put("locked", locked)
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
    }

    private fun JSONObject.str(key: String): String? = if (isNull(key)) null else optString(key)

    /** A `GET /display/targets` body; null when it is not one. Every field defaults, like the Rust model's. */
    fun parse(body: String): Targets? = runCatching {
        val o = JSONObject(body)
        val rows = o.optJSONArray("targets") ?: JSONArray()
        Targets(
            locked = o.optBoolean("locked"),
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
     * `PUT /api/v1/display/clients/{this device's fingerprint}` with `{"screen": target}`. Null
     * when the host took it, else the notice; [label] is the row's word. The caller fetches again.
     */
    fun setScreen(
        identity: ClientIdentity,
        addr: String,
        mgmtPort: Int,
        fpHex: String,
        target: DisplayTarget,
        label: String,
        streamPort: Int = 0,
    ): Say? {
        val device = certFingerprint(identity.certPem)
            ?: return say(R.string.hostcard_action_failed, label, MgmtReply.Unreachable.why)
        val hex = device.joinToString("") { "%02x".format(it) }
        val body = JSONObject().put("screen", target.json()).toString()
        val reply = mgmtCall(identity, addr, mgmtPort, fpHex, streamPort = streamPort) { base ->
            Request.Builder()
                .url("$base/api/v1/display/clients/$hex")
                .put(body.toRequestBody("application/json".toMediaType()))
                .build()
        }
        return if (reply is MgmtReply.Answer && reply.ok) null else say(R.string.hostcard_action_failed, label, reply.why)
    }
}
