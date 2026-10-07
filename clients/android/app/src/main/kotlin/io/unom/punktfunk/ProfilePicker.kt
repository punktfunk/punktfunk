package io.unom.punktfunk

import android.util.Log
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.ListedProfile
import io.unom.punktfunk.kit.ProfilePick
import io.unom.punktfunk.kit.SeatGate
import io.unom.punktfunk.kit.initials
import io.unom.punktfunk.kit.library.mgmtBase
import io.unom.punktfunk.kit.library.mtlsHttpClient
import io.unom.punktfunk.kit.pickerDecision
import io.unom.punktfunk.kit.seatGate
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.kit.wakingLine
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody

/** What `GET /api/v1/profiles/enumerate` said. */
sealed interface ProfilesAnswer {
    data class Listed(val rows: List<ListedProfile>) : ProfilesAnswer
    data object NoProfiles : ProfilesAnswer
    data class Failed(val why: String) : ProfilesAnswer
}

/** The wait before a connect dials without an answer. */
private const val PROFILES_WAIT_MS = 3_000L

/** The gap between two looks at a seat that is coming up. */
private const val SEAT_POLL_MS = 2_000L

/** Asks a paired host who can play on it, over the same mTLS client as [HostActions]. Blocking. */
object HostProfiles {
    private const val TAG = "HostProfiles"

    private fun client(identity: ClientIdentity, addr: String, fpHex: String) =
        mtlsHttpClient(identity.certPem, identity.privateKeyPem, addr, fpHex)
            .newBuilder().callTimeout(PROFILES_WAIT_MS, TimeUnit.MILLISECONDS).build()

    fun fetch(identity: ClientIdentity, addr: String, mgmtPort: Int, fpHex: String): ProfilesAnswer =
        runCatching {
            val req = Request.Builder().url("${mgmtBase(addr, mgmtPort)}/api/v1/profiles/enumerate").get().build()
            client(identity, addr, fpHex).newCall(req).execute().use { resp ->
                when {
                    resp.code == 404 -> ProfilesAnswer.NoProfiles
                    !resp.isSuccessful -> ProfilesAnswer.Failed("the host answered ${resp.code}")
                    else -> ListedProfile.parseList(resp.body?.string().orEmpty())
                        ?.let { ProfilesAnswer.Listed(it) } ?: ProfilesAnswer.Failed("bad answer")
                }
            }
        }.getOrElse { ProfilesAnswer.Failed(it.message ?: "no answer") }

    /** Starts a stopped seat. `enumerate` says whether it came up, so a failure is only logged. */
    fun wake(identity: ClientIdentity, addr: String, mgmtPort: Int, fpHex: String, id: String) {
        runCatching {
            val req = Request.Builder().url("${mgmtBase(addr, mgmtPort)}/api/v1/profiles/$id/wake")
                .post(ByteArray(0).toRequestBody(null, 0, 0)).build()
            client(identity, addr, fpHex).newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) Log.i(TAG, "profile seat did not wake: the host answered ${resp.code}")
            }
        }.onFailure { Log.i(TAG, "profile seat did not wake", it) }
    }
}

/** A picker a connect is waiting on: [answer] completes with the pick, or `null` for Cancel. */
class ProfileAsk(
    val host: KnownHost,
    val listed: List<ListedProfile>,
    val gone: String?,
) {
    val answer = CompletableDeferred<ProfilePick?>()
}

/** What a connect does about the profile. */
sealed interface ProfileChoice {
    /** Dial with this id (`null` sends none). */
    data class Dial(val id: String?) : ProfileChoice

    /** The player cancelled the picker or the seat wait. */
    data object Cancelled : ProfileChoice

    /** The profile can't play now. [line] is the host's own sentence. */
    data class Refused(val line: String) : ProfileChoice
}

/** A seat wait a connect shows: [title], the host's progress [detail], and Cancel. */
class ProfileWait(val title: String) {
    var detail by mutableStateOf<String?>(null)
    val cancelled = CompletableDeferred<Unit>()
}

/** Whether [host] still lists profile [id], by a fresh answer. */
suspend fun stillListed(identity: ClientIdentity, host: KnownHost, id: String): Boolean =
    withContext(Dispatchers.IO) {
        HostProfiles.fetch(identity, host.address, host.effectiveMgmtPort, host.fpHex)
    }.let { it is ProfilesAnswer.Listed && it.rows.any { r -> r.id == id } }

/** Save [pick] as [host]'s profile (or clear it); a no-op when the record is gone. */
fun KnownHostStore.savePick(host: KnownHost, pick: ProfilePick?) {
    val h = byId(host.id) ?: return
    if (h.asProfile != pick) save(h.copy(asProfile = pick))
}

/**
 * The profile a connect to [host] dials as: asks the host who plays on it, applies
 * [pickerDecision], then [seatGate] to the profile that plays. A failed or late answer dials
 * with the saved pick. [link] is a link's `as=`. [ask] shows the picker and returns once the
 * player has answered; [wait] shows the seat wait, and hides it with `null`.
 */
suspend fun chooseProfile(
    store: KnownHostStore,
    identity: ClientIdentity,
    host: KnownHost?,
    link: String?,
    wait: (ProfileWait?) -> Unit,
    ask: suspend (ProfileAsk) -> Unit,
): ProfileChoice {
    if (host == null || !host.paired || host.fpHex.isEmpty()) return ProfileChoice.Dial(link)
    val saved = store.byId(host.id)?.asProfile ?: host.asProfile
    val answer = withContext(Dispatchers.IO) {
        HostProfiles.fetch(identity, host.address, host.effectiveMgmtPort, host.fpHex)
    }
    val listed = when (answer) {
        is ProfilesAnswer.Listed -> answer.rows.takeIf { it.isNotEmpty() }
        ProfilesAnswer.NoProfiles -> null
        is ProfilesAnswer.Failed -> return ProfileChoice.Dial(link ?: saved?.id)
    }
    val d = pickerDecision(listed, saved, link)
    if (!d.picker) {
        if (d.remember != saved) store.savePick(host, d.remember)
        return gateSeat(d.send, listed, identity, host, wait)
    }
    val asked = ProfileAsk(host, listed.orEmpty(), d.gone)
    ask(asked)
    val pick = asked.answer.await() ?: return ProfileChoice.Cancelled
    store.savePick(host, pick)
    return gateSeat(pick.id, listed, identity, host, wait)
}

/**
 * Dials as [id] once its seat in [listed] allows: wakes a stopped seat and polls `enumerate` while
 * it starts. There is no timeout; Cancel is the way out.
 */
private suspend fun gateSeat(
    id: String?,
    listed: List<ListedProfile>?,
    identity: ClientIdentity,
    host: KnownHost,
    wait: (ProfileWait?) -> Unit,
): ProfileChoice {
    val row = listed?.firstOrNull { it.id == id } ?: return ProfileChoice.Dial(id)
    val w = ProfileWait(wakingLine(row.displayName))
    wait(w)
    try {
        var gate = seatGate(row)
        while (true) {
            when (val g = gate) {
                SeatGate.Dial -> return ProfileChoice.Dial(id)
                is SeatGate.Refuse -> return ProfileChoice.Refused(g.line)
                SeatGate.Wake -> withContext(Dispatchers.IO) {
                    HostProfiles.wake(identity, host.address, host.effectiveMgmtPort, host.fpHex, row.id)
                }
                is SeatGate.Wait -> w.detail = g.detail
            }
            if (withTimeoutOrNull(SEAT_POLL_MS) { w.cancelled.await() } != null) return ProfileChoice.Cancelled
            val answer = withContext(Dispatchers.IO) {
                HostProfiles.fetch(identity, host.address, host.effectiveMgmtPort, host.fpHex)
            }
            if (w.cancelled.isCompleted) return ProfileChoice.Cancelled
            val rows = (answer as? ProfilesAnswer.Listed)?.rows ?: continue
            // A profile deleted mid-wait dials anyway: the host's own refusal says so.
            gate = rows.firstOrNull { it.id == row.id }?.let(::seatGate) ?: return ProfileChoice.Dial(id)
        }
    } finally {
        wait(null)
    }
}

/** The seat wait: the title, the host's progress line, Cancel. Back cancels too. */
@Composable
internal fun SeatWaitDialog(wait: ProfileWait, onCancel: () -> Unit) {
    AlertDialog(
        onDismissRequest = onCancel,
        title = { Text(wait.title) },
        text = {
            Column {
                wait.detail?.let {
                    Text(it, style = MaterialTheme.typography.bodyMedium)
                    Spacer(Modifier.height(12.dp))
                }
                CircularProgressIndicator()
            }
        },
        confirmButton = { TextButton(onClick = onCancel) { Text("Cancel") } },
    )
}

/** The accent colour of a profile, or the app accent when it has none. */
@Composable
private fun accentOf(accent: String?): Color =
    accent?.let { runCatching { Color(android.graphics.Color.parseColor(it)) }.getOrNull() }
        ?: MaterialTheme.colorScheme.primary

/** A circle with a name's initials: the profile's face, and the small mark on a host card. */
@Composable
internal fun ProfileFace(name: String, size: Int, accent: String? = null, ringed: Boolean = false) {
    val color = accentOf(accent)
    Box(
        Modifier
            .size(size.dp)
            .clip(CircleShape)
            .background(color)
            .then(
                if (ringed) Modifier.border(BorderStroke(3.dp, MaterialTheme.colorScheme.onSurface), CircleShape)
                else Modifier,
            ),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            initials(name),
            color = Color.White,
            style = if (size >= 48) MaterialTheme.typography.titleLarge else MaterialTheme.typography.labelSmall,
        )
    }
}

/**
 * Who plays on [hostName]: one circle per profile, the saved pick first and ringed. [answer] is
 * `null` while the host is asked. A pick calls [onPick]; the dialog never saves anything itself.
 */
@Composable
internal fun ProfilePickerDialog(
    hostName: String,
    answer: ProfilesAnswer?,
    saved: ProfilePick?,
    gone: String?,
    onPick: (ProfilePick) -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Who’s playing on $hostName?") },
        text = {
            Column {
                if (gone != null) {
                    Text(
                        "$gone is gone from this host.",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.error,
                    )
                    Spacer(Modifier.height(12.dp))
                }
                when (answer) {
                    null -> CircularProgressIndicator()
                    ProfilesAnswer.NoProfiles -> Text("No profiles on this host.")
                    is ProfilesAnswer.Failed -> Text("Couldn’t load the profiles.")
                    is ProfilesAnswer.Listed -> {
                        if (answer.rows.isEmpty()) {
                            Text("No profiles on this host.")
                        } else {
                            val rows = answer.rows.sortedByDescending { it.id == saved?.id }
                            Row(
                                Modifier.horizontalScroll(rememberScrollState()),
                                horizontalArrangement = Arrangement.spacedBy(16.dp),
                            ) {
                                rows.forEach { p ->
                                    Column(
                                        Modifier
                                            .width(88.dp)
                                            .clip(MaterialTheme.shapes.medium)
                                            .clickable { onPick(p.pick()) }
                                            .padding(4.dp),
                                        horizontalAlignment = Alignment.CenterHorizontally,
                                    ) {
                                        ProfileFace(p.displayName, 72, p.accent, ringed = p.id == saved?.id)
                                        Spacer(Modifier.height(6.dp))
                                        Text(
                                            p.displayName,
                                            style = MaterialTheme.typography.bodyMedium,
                                            maxLines = 1,
                                            overflow = TextOverflow.Ellipsis,
                                        )
                                        p.note()?.let {
                                            Text(
                                                it,
                                                style = MaterialTheme.typography.labelSmall,
                                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                                                textAlign = TextAlign.Center,
                                                maxLines = 2,
                                                overflow = TextOverflow.Ellipsis,
                                            )
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
        confirmButton = {
            if (answer !is ProfilesAnswer.Listed || answer.rows.isEmpty()) {
                TextButton(onClick = onDismiss) { Text("Close") }
            }
        },
        dismissButton = {
            if (answer is ProfilesAnswer.Listed && answer.rows.isNotEmpty()) {
                TextButton(onClick = onDismiss) { Text("Cancel") }
            }
        },
    )
}
