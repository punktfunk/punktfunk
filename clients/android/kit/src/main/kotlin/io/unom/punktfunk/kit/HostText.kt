package io.unom.punktfunk.kit

import android.content.res.Resources
import org.json.JSONObject

/**
 * A sentence the host sent by the id of its message, worded in the app's language.
 *
 * The host names the message beside its English: an API error's `code` and `params`, a seat
 * line's `detail_id`, a launch verdict's id. An id this build has no message for, such as a newer
 * host's, reads as the host's own English. `clients/shared/host-text-vectors.json` pins the
 * sentences for every client.
 */
object HostText {
    /** What a host may ask this app to say. Any other message is the app's own. */
    private val IDS = listOf("apierr_", "launch_", "seatline_", "reject_")

    /**
     * The messages with several placeholders, by the names they take in the order the English
     * shows them. Any other message holds at most one, which the host sends as its one value.
     */
    private val ORDER = mapOf(
        "launch_closed_at_once" to listOf("title", "seconds"),
        "launch_download_failed_reason" to listOf("title", "reason"),
        "launch_download_refused" to listOf("title", "reason"),
        "seatline_stage_display_try" to listOf("attempt", "total"),
        "seatline_stage_host_try" to listOf("attempt", "total"),
        "seatline_stage_sign_in_try" to listOf("attempt", "total"),
    )

    /** The message [id] names, filled from [params]; null when this build has none to fill. */
    fun words(res: Resources, id: String, params: Map<String, String>): String? {
        if (IDS.none { id.startsWith(it) }) return null
        // The generated resources share one package; a message the build has resolves in it.
        val pkg = res.getResourcePackageName(R.string.reject_busy)
        val order = ORDER[id]
        if (order == null && params.size > 1) return null
        val plural = res.getIdentifier(id, "plurals", pkg)
        if (plural != 0) {
            val count = params["count"]?.toIntOrNull() ?: return null
            val args = (order ?: listOf("count")).map<String, Any> { if (it == "count") count else params[it] ?: return null }
            return runCatching { res.getQuantityString(plural, count, *args.toTypedArray()) }.getOrNull()
        }
        val string = res.getIdentifier(id, "string", pkg)
        if (string == 0) return null
        val args: List<Any> = order?.map { params[it] ?: return null } ?: params.values.toList()
        // A message whose placeholders the host's values do not fill throws; that reads as English.
        return runCatching { res.getString(string, *args.toTypedArray()) }.getOrNull()
    }

    /**
     * The sentence of an `ApiError` body (`{"error", "code", "params"}`): the host's message
     * when it names one, else its English. Null for a body that is not one or says nothing.
     */
    fun apiError(body: String): Say? {
        val json = runCatching { JSONObject(body) }.getOrNull() ?: return null
        val english = json.optString("error")
        val code = json.optString("code").takeIf { it.isNotBlank() }
        return when {
            code != null -> Say.Host("apierr_$code", paramsOf(json.optJSONObject("params")), english)
            english.isNotBlank() -> Say.Text(english)
            else -> null
        }
    }

    /** The values of a message's placeholders, by name; a value that is not a string is left out. */
    fun paramsOf(json: JSONObject?): Map<String, String> {
        if (json == null) return emptyMap()
        return json.keys().asSequence().mapNotNull { k -> (json.opt(k) as? String)?.let { k to it } }.toMap()
    }
}
