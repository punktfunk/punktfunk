package io.unom.punktfunk

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
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.ListedProfile
import io.unom.punktfunk.kit.ProfilePick
import io.unom.punktfunk.kit.initials
import io.unom.punktfunk.kit.library.mgmtBase
import io.unom.punktfunk.kit.library.mtlsHttpClient
import io.unom.punktfunk.kit.pickerDecision
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.Request

/** What `GET /api/v1/profiles/enumerate` said. */
sealed interface ProfilesAnswer {
    data class Listed(val rows: List<ListedProfile>) : ProfilesAnswer
    data object NoProfiles : ProfilesAnswer
    data class Failed(val why: String) : ProfilesAnswer
}

/** The wait before a connect dials without an answer. */
private const val PROFILES_WAIT_MS = 3_000L

/** Asks a paired host who can play on it, over the same mTLS client as [HostActions]. Blocking. */
object HostProfiles {
    fun fetch(identity: ClientIdentity, addr: String, mgmtPort: Int, fpHex: String): ProfilesAnswer =
        runCatching {
            val client = mtlsHttpClient(identity.certPem, identity.privateKeyPem, addr, fpHex)
                .newBuilder().callTimeout(PROFILES_WAIT_MS, TimeUnit.MILLISECONDS).build()
            val req = Request.Builder().url("${mgmtBase(addr, mgmtPort)}/api/v1/profiles/enumerate").get().build()
            client.newCall(req).execute().use { resp ->
                when {
                    resp.code == 404 -> ProfilesAnswer.NoProfiles
                    !resp.isSuccessful -> ProfilesAnswer.Failed("the host answered ${resp.code}")
                    else -> ListedProfile.parseList(resp.body?.string().orEmpty())
                        ?.let { ProfilesAnswer.Listed(it) } ?: ProfilesAnswer.Failed("bad answer")
                }
            }
        }.getOrElse { ProfilesAnswer.Failed(it.message ?: "no answer") }
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

    /** The player cancelled the picker. */
    data object Cancelled : ProfileChoice
}

/** Save [pick] as [host]'s profile (or clear it); a no-op when the record is gone. */
fun KnownHostStore.savePick(host: KnownHost, pick: ProfilePick?) {
    val h = byId(host.id) ?: return
    if (h.asProfile != pick) save(h.copy(asProfile = pick))
}

/**
 * The profile a connect to [host] dials as: asks the host who plays on it, then applies
 * [pickerDecision]. A failed or late answer dials with the saved pick. [link] is a link's `as=`.
 * [ask] shows the picker and returns once the player has answered.
 */
suspend fun chooseProfile(
    store: KnownHostStore,
    identity: ClientIdentity,
    host: KnownHost?,
    link: String?,
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
        return ProfileChoice.Dial(d.send)
    }
    val asked = ProfileAsk(host, listed.orEmpty(), d.gone)
    ask(asked)
    val pick = asked.answer.await() ?: return ProfileChoice.Cancelled
    store.savePick(host, pick)
    return ProfileChoice.Dial(pick.id)
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
