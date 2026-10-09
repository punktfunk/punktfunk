package io.unom.punktfunk

import io.unom.punktfunk.kit.library.MgmtReply
import io.unom.punktfunk.kit.library.mgmtCall
import io.unom.punktfunk.kit.security.ClientIdentity
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject

/**
 * Host actions — sleep, restart or shut down a paired host from this device
 * (`design/host-actions.md` §7), over the same mTLS identity the library fetch and the log
 * upload use.
 *
 * The HOST is the only enforcer: [Action.permitted] is what it says about *this* device's
 * access, so a device without the Host-power grant is offered nothing rather than shown a row
 * that will be refused. Discovery is best-effort by contract — an older host (no such route),
 * an unreachable one, or a shape we don't recognise yields an empty list, because a missing
 * menu row costs a menu row and a thrown exception costs the screen.
 *
 * ONE implementation for both Android shells, like [SendLogs]: the Skia console's host menu and
 * the touch home's card menu. Wording is the desktop's verbatim (`pf_client_core::host_actions`)
 * so a quoted message means the same thing on every client.
 *
 * Blocking — call it off the main thread.
 */
object HostActions {
    /** One action as the host reports it to THIS device. */
    data class Action(
        /** Stable id, the invoke argument (`power.sleep`). */
        val id: String,
        /** This client's wording for a known id, else the host's own title. */
        val label: String,
        /** Confirm twice — the action loses whatever is running on that machine. */
        val danger: Boolean,
        /** The host can run it right now. */
        val available: Boolean,
        /** Why not, when it can't. Empty otherwise. */
        val unavailableReason: String,
    )

    /** Local wording for the ids we know; anything else keeps the host's own title, which is
     *  what lets a later host add an action with no client release. */
    private fun label(id: String, title: String): String = when (id) {
        "power.sleep" -> "Sleep host"
        "power.reboot" -> "Restart host"
        "power.shutdown" -> "Shut down host"
        else -> title
    }

    /**
     * What this host lets this device do to it (`GET /api/v1/actions`). Only the PERMITTED rows
     * come back: what a device may not invoke is not its business to render.
     */
    fun list(identity: ClientIdentity, addr: String, mgmtPort: Int, fpHex: String): List<Action> {
        val reply = mgmtCall(identity, addr, mgmtPort, fpHex) { base ->
            Request.Builder().url("$base/api/v1/actions").get().build()
        }
        if (reply !is MgmtReply.Answer || !reply.ok) return emptyList()
        val arr = runCatching { JSONObject(reply.body).optJSONArray("actions") }.getOrNull()
            ?: return emptyList()
        return (0 until arr.length()).mapNotNull { i ->
            val o = arr.optJSONObject(i) ?: return@mapNotNull null
            if (!o.optBoolean("permitted")) return@mapNotNull null
            val id = o.optString("id")
            Action(
                id = id,
                label = label(id, o.optString("title")),
                danger = o.optBoolean("danger"),
                available = o.optBoolean("available"),
                unavailableReason = o.optString("unavailable_reason"),
            )
        }
    }

    /**
     * Invoke one action by id (`POST /api/v1/actions/{id}`, empty body) and return the
     * user-facing outcome.
     *
     * A 2xx is the last word: the host ends every session and acts about a second later, so
     * there is nothing to poll and nothing to undo. A refusal carries the host's own reason
     * ("another device is streaming from this host right now"), which tells a person what to do
     * where a bare status code would not.
     */
    fun invoke(
        identity: ClientIdentity,
        addr: String,
        mgmtPort: Int,
        fpHex: String,
        hostName: String,
        actionId: String,
        label: String,
    ): String {
        val reply = mgmtCall(identity, addr, mgmtPort, fpHex) { base ->
            Request.Builder()
                .url("$base/api/v1/actions/$actionId")
                // Empty body by design: the id is the whole request, and no request field ever
                // reaches the host's privileged path.
                .post(ByteArray(0).toRequestBody(null, 0, 0))
                .build()
        }
        return if (reply is MgmtReply.Answer && reply.ok) {
            "$hostName: $label — on its way"
        } else {
            "$label failed — ${reply.why}"
        }
    }
}
