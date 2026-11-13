package io.unom.punktfunk.kit

import org.json.JSONArray
import org.json.JSONObject

/**
 * Profiles on a box: what `GET /api/v1/profiles/enumerate` lists, when a client shows the picker,
 * and which id its hello carries. A port of `pf-client-core`'s `profiles.rs`;
 * `clients/shared/profile-picker-vectors.json` pins [pickerDecision] in both, and
 * `profile-unlock-vectors.json` pins [UnlockNeed.of].
 */

/**
 * The pick a device saves for a box. Shown on the card, so it carries the name and [avatar], the
 * picture's host-relative URL. [remember]: the player asked this device to keep it; the console
 * applies only such a pick.
 */
data class ProfilePick(
    val id: String,
    val displayName: String = "",
    val remember: Boolean = false,
    val avatar: String? = null,
) {
    /** `{id, display_name, avatar, remember}` as `pf-client-core` spells it; the last two when set. */
    fun toJson(): JSONObject = JSONObject().put("id", id).put("display_name", displayName)
        .also {
            if (avatar != null) it.put("avatar", avatar)
            if (remember) it.put("remember", true)
        }

    companion object {
        /** [toJson]'s shape back; `null` without an id. */
        fun fromJson(j: JSONObject?): ProfilePick? = j
            ?.let {
                val avatar = if (it.isNull("avatar")) null else it.optString("avatar")
                ProfilePick(it.optString("id"), it.optString("display_name"), it.optBoolean("remember"), avatar)
            }
            ?.takeIf { it.id.isNotEmpty() }
    }
}

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

/**
 * What a profile on a person's own account or a locked one needs from this device before it
 * plays: nothing, the account password, the profile's PIN, or a proof from this device's unlock
 * key ([KEY] else the password, [PIN_KEY] else the PIN). An old host sends none: [NONE]. A word a
 * newer host adds reads as [PASSWORD], the prompt every client has.
 */
enum class UnlockNeed {
    NONE, PASSWORD, KEY, PIN, PIN_KEY;

    /** The prompt asks for the profile's PIN, not an account password. */
    val pin: Boolean get() = this == PIN || this == PIN_KEY

    companion object {
        fun of(word: String?): UnlockNeed = when (word) {
            null, "none" -> NONE
            "key" -> KEY
            "pin" -> PIN
            "pin_key" -> PIN_KEY
            else -> PASSWORD
        }
    }
}

/** A profile's own seat. Absent for one that plays on the box's own session. */
data class Seat(
    val state: SeatState = SeatState.READY,
    /** The progress line while starting, or why while unavailable: the host's English. */
    val detail: String? = null,
    /** The host message of [detail] (`seatline_*`), absent for the supervisor's own text. */
    val detailId: String? = null,
    /** The values of that message's placeholders, by name. */
    val detailParams: Map<String, String> = emptyMap(),
    /** The device playing on it while occupied. */
    val occupant: String? = null,
    /** Its Steam has no account yet. `null` where the host can't tell. */
    val steamSignIn: Boolean? = null,
) {
    /** [detail] in the app's language when the host names its message, else as the host said it. */
    val line: Say?
        get() = detail?.let { if (detailId != null) Say.Host(detailId, detailParams, it) else Say.Text(it) }
}

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
    /** Host-relative URL of the picture. */
    val avatar: String? = null,
    /** What this device must send to play as it, as the host judged for this device. */
    val unlock: UnlockNeed = UnlockNeed.NONE,
) {
    fun pick() = ProfilePick(id, displayName, avatar = avatar)

    /** The one line under a picker card, if any. */
    fun note(): Say? {
        val seat = seat ?: return null
        return when {
            seat.state == SeatState.OCCUPIED ->
                seat.occupant?.let { say(R.string.profile_in_use_by, it) } ?: say(R.string.profile_in_use)
            seat.state == SeatState.STARTING -> seat.line ?: say(R.string.profile_getting_ready)
            seat.state == SeatState.UNAVAILABLE -> seat.line ?: say(R.string.profile_unavailable)
            seat.steamSignIn == true -> say(R.string.profile_steam_sign_in)
            else -> null
        }
    }

    companion object {
        private fun str(o: JSONObject, k: String) = if (o.isNull(k)) null else o.optString(k)

        fun parse(j: JSONObject) = ListedProfile(
            id = j.optString("id", ""),
            displayName = j.optString("display_name", ""),
            accent = str(j, "accent"),
            avatar = str(j, "avatar"),
            owner = j.optBoolean("owner", false),
            seat = j.optJSONObject("seat")?.let {
                Seat(
                    state = SeatState.of(str(it, "state")),
                    detail = str(it, "detail"),
                    detailId = str(it, "detail_id"),
                    detailParams = HostText.paramsOf(it.optJSONObject("detail_params")),
                    occupant = str(it, "occupant"),
                    steamSignIn = if (it.isNull("steam_sign_in")) null else it.optBoolean("steam_sign_in"),
                )
            },
            legacySeat = j.optBoolean("legacy_seat", false),
            unlock = UnlockNeed.of(str(j, "unlock")),
        )

        /** The body of `enumerate`, or `null` when it isn't a JSON array. */
        fun parseList(body: String): List<ListedProfile>? = runCatching {
            JSONArray(body).let { a -> (0 until a.length()).map { parse(a.getJSONObject(it)) } }
        }.getOrNull()
    }
}

/**
 * [path], a row's `avatar`, on the host at [base]. `null` for anything but a path: the poster
 * client presents this device's certificate to the origin it is handed.
 */
fun pictureUrl(base: String, path: String?): String? = path?.takeIf { it.startsWith("/") }?.let { base + it }

/** Initials for a profile without a picture: the first letters of its first two words. */
fun initials(name: String): String = name.trim().split(Regex("\\s+")).filter { it.isNotEmpty() }
    .take(2).joinToString("") { firstLetter(it) }

/** The first character of a non-empty [word], upper-cased. A character past U+FFFF stays whole. */
fun firstLetter(word: String): String = word.substring(0, word.offsetByCodePoints(0, 1)).uppercase()

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
    // A kept pick keeps `remember`: saving it back never clears the console's.
    val kept = still?.pick()?.copy(remember = remembered?.remember == true)
    if (link != null) {
        val p = findProfile(listed, link)
        return if (p != null) {
            ProfileDecision(send = p.id, remember = kept)
        } else {
            ProfileDecision(picker = true, remember = kept)
        }
    }
    if (listed.size == 1) return ProfileDecision(send = listed[0].id, remember = kept)
    if (still != null) return ProfileDecision(send = still.id, remember = kept)
    if (remembered == null) {
        listed.firstOrNull { it.legacySeat }?.let {
            return ProfileDecision(send = it.id, remember = it.pick())
        }
    }
    return ProfileDecision(picker = true, gone = remembered?.displayName)
}

/** What a client does with a picked profile's seat before it dials. */
sealed interface SeatGate {
    /** Dial now. The host places the connect, or says why it can't. */
    data object Dial : SeatGate

    /** `POST /api/v1/profiles/{id}/wake`, then [Wait]. */
    data object Wake : SeatGate

    /** The seat is coming up: poll `enumerate`, show [detail], offer Cancel. */
    data class Wait(val detail: Say?) : SeatGate

    /** The seat can't play now; [line] says why. Don't dial. */
    data class Refuse(val line: Say) : SeatGate
}

/**
 * The gate for [p]'s seat, as `enumerate` lists it. A locked seat that is stopped dials: the
 * host refuses its wake, and the unlock on the dial starts it.
 */
fun seatGate(p: ListedProfile): SeatGate {
    val seat = p.seat ?: return SeatGate.Dial
    return when (seat.state) {
        SeatState.READY, SeatState.OCCUPIED, SeatState.OTHER -> SeatGate.Dial
        SeatState.STOPPED -> if (p.unlock == UnlockNeed.NONE) SeatGate.Wake else SeatGate.Dial
        SeatState.STARTING -> SeatGate.Wait(seat.line)
        SeatState.UNAVAILABLE -> SeatGate.Refuse(seat.line ?: say(R.string.reject_seat_unavailable))
    }
}

/** The line while a seat comes up: `Getting Kid's desk ready…`. */
fun wakingLine(displayName: String): Say = say(R.string.profile_waking, displayName)
