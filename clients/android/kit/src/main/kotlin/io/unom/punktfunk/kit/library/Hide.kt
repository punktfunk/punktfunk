package io.unom.punktfunk.kit.library

import android.util.Log
import io.unom.punktfunk.kit.ListedProfile
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.say
import io.unom.punktfunk.kit.security.ClientIdentity
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import java.net.URLEncoder

/**
 * "Hide for me": a player takes a title off the shelf of the locked profile this device is let
 * into, and brings it back. `enumerate` marks such a row `person`; the action exists only on the
 * saved pick's row so marked. A port of `pf-client-core`'s `hide.rs`;
 * `clients/shared/hide-for-me-vectors.json` pins it there, here and in Swift.
 */
object Hide {
    private const val TAG = "Hide"

    /**
     * The row of the profile the shelf plays as ([pick]) when this device may hide its titles.
     * [listed] is what `enumerate` answered, null for a host without profiles.
     */
    fun offer(listed: List<ListedProfile>?, pick: String?): ListedProfile? =
        pick?.let { p -> listed?.firstOrNull { it.id == p && it.person } }

    /** The path and JSON body of the `PUT` that hides ([hidden]) or shows title [id] for [profile]. */
    internal fun request(profile: String, id: String, hidden: Boolean): Pair<String, String> =
        "/api/v1/library/hidden/${LibraryClient.pathSegment(id)}?as=${query(profile)}" to """{"hidden":$hidden}"""

    /**
     * What [profile] hides, its person asking: every page of `hidden=true`, keeping only rows
     * marked hidden, so a host that ignores the flag lists nothing. [get] answers one path. Never
     * cached.
     */
    internal fun hiddenList(profile: String, base: String, get: (path: String) -> Pair<Int, String>): LibraryResult {
        val walked = LibraryClient.walkPages(base, keep = { it.optBoolean("hidden") }) { cursor ->
            val page = "/api/v1/library/page?limit=${LibraryClient.PAGE_LIMIT}&hidden=true" +
                (cursor?.let { "&cursor=" + query(it) } ?: "")
            get("$page&as=${query(profile)}")
        }
        return when (walked) {
            is LibraryClient.Walk.Done -> LibraryResult.Ok(walked.games, profile)
            is LibraryClient.Walk.Refused -> LibraryClient.refused(walked.code)
        }
    }

    /** Hide ([hidden]) or show title [appId] for [profile] on a paired host. BLOCKING. */
    fun set(
        id: ClientIdentity,
        address: String,
        mgmtPort: Int,
        fpHex: String,
        profile: String,
        appId: String,
        hidden: Boolean,
        streamPort: Int = 0,
    ): HideOutcome {
        val (path, body) = request(profile, appId, hidden)
        val reply = mgmtCall(id, address, mgmtPort, fpHex, streamPort = streamPort) { base ->
            Request.Builder().url(base + path).put(body.toRequestBody("application/json".toMediaType())).build()
        }
        return when (reply) {
            is MgmtReply.Answer -> HideOutcome.fromReply(reply.code, reply.apiError)
            MgmtReply.Unreachable -> HideOutcome.Failed(reply.why)
        }
    }

    /** [hiddenList] over [mtlsHttpClient]'s lanes. BLOCKING. */
    fun fetch(
        id: ClientIdentity,
        address: String,
        mgmtPort: Int,
        fpHex: String,
        profile: String,
        streamPort: Int = 0,
    ): LibraryResult {
        if (fpHex.isBlank()) return LibraryResult.Unauthorized(say(R.string.android_lib_error_unpaired_first))
        val base = mgmtBase(address, mgmtPort)
        return try {
            val client = mtlsHttpClient(id.certPem, id.privateKeyPem, address, fpHex, streamPort)
            hiddenList(profile, base) { path ->
                client.newCall(Request.Builder().url(base + path).build()).execute().use { it.code to it.body.string() }
            }
        } catch (e: Exception) {
            Log.w(TAG, "hidden titles from $base", e)
            LibraryResult.Error(say(R.string.android_lib_error_unreachable))
        }
    }

    private fun query(s: String): String = URLEncoder.encode(s, "UTF-8")
}

/** What a hide or a show came to: the Rust `hide::Outcome`. */
sealed class HideOutcome {
    data object Done : HideOutcome()

    /** The host's sentence, worded by its code where this build has the message. */
    data class Refused(val said: Say) : HideOutcome()

    /** No sentence to show: the cause, for this app's own line. */
    data class Failed(val why: Say) : HideOutcome()

    /** The line a player reads after hiding ([hidden]) or showing [title] for profile [name]. */
    fun notice(hidden: Boolean, title: String, name: String): Say = when (this) {
        Done -> say(if (hidden) R.string.lib_hidden_done else R.string.lib_shown_done, title, name)
        is Refused -> said
        is Failed -> say(if (hidden) R.string.lib_hide_failed else R.string.lib_show_failed, title, why)
    }

    companion object {
        /**
         * [said] is [HostText.apiError]'s. Only a coded error ([Say.Host]) is a sentence for the
         * player; one with no code is the operator's line and reads as a failure.
         */
        fun fromReply(code: Int, said: Say?): HideOutcome = when {
            code in 200..299 -> Done
            said is Say.Host -> Refused(said)
            else -> Failed(say(R.string.error_mgmt_refused, code))
        }
    }
}
