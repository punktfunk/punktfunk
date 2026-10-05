package io.unom.punktfunk.kit

import org.json.JSONArray
import org.json.JSONObject

/**
 * Profiles on a box: what `GET /api/v1/profiles/enumerate` lists, when a client shows the picker,
 * and which id its hello carries. A port of `pf-client-core`'s `profiles.rs`;
 * `clients/shared/profile-picker-vectors.json` pins [pickerDecision] in both.
 */

/** The pick a device remembers for a box. Shown on the card, so it carries the name. */
data class ProfilePick(val id: String, val displayName: String = "")

/** A seat profile's state right now. A word the host adds later reads as [OTHER]. */
enum class SeatState {
    READY, STARTING, STOPPED, OCCUPIED, UNAVAILABLE, OTHER;

    companion object {
        fun of(word: String?): SeatState = when (word) {
            null, "ready" -> READY
            "starting" -> STARTING
            "stopped" -> STOPPED
            "occupied" -> OCCUPIED
            "unavailable" -> UNAVAILABLE
            else -> OTHER
        }
    }
}

/** A profile's own seat. Absent for one that plays on the box's own session. */
data class Seat(
    val state: SeatState = SeatState.READY,
    /** The progress line while starting, or why while unavailable. */
    val detail: String? = null,
    /** The device playing on it while occupied. */
    val occupant: String? = null,
    /** Its Steam has no account yet. `null` where the host can't tell. */
    val steamSignIn: Boolean? = null,
)

/** One row of `enumerate`. Every field defaults, so a host that adds one never fails the list. */
data class ListedProfile(
    val id: String = "",
    val displayName: String = "",
    /** `#RRGGBB` behind the initials. */
    val accent: String? = null,
    val owner: Boolean = false,
    val seat: Seat? = null,
    /** This device's old seat became this profile. */
    val legacySeat: Boolean = false,
) {
    fun pick() = ProfilePick(id, displayName)

    /** The one line under a picker card, if any. */
    fun note(): String? {
        val seat = seat ?: return null
        return when {
            seat.state == SeatState.OCCUPIED ->
                seat.occupant?.let { "In use by $it" } ?: "In use"
            seat.state == SeatState.STARTING -> seat.detail ?: "Getting ready…"
            seat.state == SeatState.UNAVAILABLE -> seat.detail ?: "Unavailable"
            seat.steamSignIn == true -> "Steam sign-in once"
            else -> null
        }
    }

    companion object {
        private fun str(o: JSONObject, k: String) = if (o.isNull(k)) null else o.optString(k)

        fun parse(j: JSONObject) = ListedProfile(
            id = j.optString("id", ""),
            displayName = j.optString("display_name", ""),
            accent = str(j, "accent"),
            owner = j.optBoolean("owner", false),
            seat = j.optJSONObject("seat")?.let {
                Seat(
                    state = SeatState.of(str(it, "state")),
                    detail = str(it, "detail"),
                    occupant = str(it, "occupant"),
                    steamSignIn = if (it.isNull("steam_sign_in")) null else it.optBoolean("steam_sign_in"),
                )
            },
            legacySeat = j.optBoolean("legacy_seat", false),
        )

        /** The body of `enumerate`, or `null` when it isn't a JSON array. */
        fun parseList(body: String): List<ListedProfile>? = runCatching {
            JSONArray(body).let { a -> (0 until a.length()).map { parse(a.getJSONObject(it)) } }
        }.getOrNull()
    }
}

/** Initials for a profile without a picture: the first letters of its first two words. */
fun initials(name: String): String = name.trim().split(Regex("\\s+")).filter { it.isNotEmpty() }
    .take(2).joinToString("") { it.substring(0, it.offsetByCodePoints(0, 1)).uppercase() }

/** The profile [wanted] names in [listed]: its id, else its name in any case. */
fun findProfile(listed: List<ListedProfile>, wanted: String): ListedProfile? =
    listed.firstOrNull { it.id == wanted } ?: listed
        .filter { it.displayName.equals(wanted, ignoreCase = true) }
        // Two profiles can't share a name, but a hand-edited file could: refuse to guess.
        .singleOrNull()

/** What a client does before it dials. */
data class ProfileDecision(
    /** Show the picker; the connect waits for a pick. */
    val picker: Boolean = false,
    /** The id the hello carries. `null` sends none. */
    val send: String? = null,
    /** The name of a remembered profile the box no longer lists, for the picker's line. */
    val gone: String? = null,
    /** The saved pick afterwards. */
    val remember: ProfilePick? = null,
)

/**
 * When to show the picker and what to dial with. [listed] is `null` for a box without profiles.
 * [link] is a link's `as=`: it wins for this connect and leaves the saved pick alone.
 */
fun pickerDecision(
    listed: List<ListedProfile>?,
    remembered: ProfilePick?,
    link: String?,
): ProfileDecision {
    if (listed == null) return ProfileDecision(send = link, remember = remembered)
    val still = remembered?.let { r -> listed.firstOrNull { it.id == r.id } }
    if (link != null) {
        val p = findProfile(listed, link)
        return if (p != null) {
            ProfileDecision(send = p.id, remember = still?.pick())
        } else {
            ProfileDecision(picker = true, remember = still?.pick())
        }
    }
    if (listed.size == 1) return ProfileDecision(send = listed[0].id, remember = still?.pick())
    if (still != null) return ProfileDecision(send = still.id, remember = still.pick())
    if (remembered == null) {
        listed.firstOrNull { it.legacySeat }?.let {
            return ProfileDecision(send = it.id, remember = it.pick())
        }
    }
    return ProfileDecision(picker = true, gone = remembered?.displayName)
}
