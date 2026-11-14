package io.unom.punktfunk

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.withStarted
import io.unom.punktfunk.components.resolveOsIcon
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.library.LibraryClient
import io.unom.punktfunk.kit.link.StartScreen
import io.unom.punktfunk.kit.security.IdentityHolder
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.models.ActiveSession
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext

/**
 * One shelf the Library tab can show: a paired host, or one of its pinned host+preset cards, whose
 * launches run with that preset.
 */
internal data class Shelf(val host: KnownHost, val preset: StreamPreset? = null) {
    val id: String get() = id(host.id, preset?.id)

    /** The host, and the preset after it for a pinned card's shelf: the card's own shape. */
    val title: String get() = preset?.let { "${host.name} · ${it.name}" } ?: host.name

    companion object {
        fun id(hostId: String, presetId: String?): String = presetId?.let { "$hostId#$it" } ?: hostId
    }
}

/** How often the tab asks which hosts answer and what they have up. */
private const val PRESENCE_LAP_MS = 30_000L

/**
 * The Library tab: one shelf, picked from the chips over it. [LibraryScreen] owns the shelf's
 * fetch, cache and launch; this picks the shelf, lists every paired host's desktop, and says what
 * to do with no paired host.
 */
@Composable
internal fun LibraryTab(
    settings: Settings,
    /** The shelf asked for by Browse library, Start in or a game's exit; null is the last shown. */
    shelfId: String?,
    onShelf: (String) -> Unit,
    /** Stream the shelf's desktop as it opens (`start_in = stream`). */
    autoStream: Boolean,
    lnpGranted: Boolean,
    onLaunched: (ActiveSession) -> Unit,
    onShowHosts: () -> Unit,
) {
    // Back leaves for the Hosts tab, as from Settings; the shelf's own handlers come first.
    BackHandler(onBack = onShowHosts)
    val context = LocalContext.current
    val store = remember { KnownHostStore(context) }
    val presetStore = remember { PresetStore(context) }
    // The fetch authenticates with the pinned identity, so only a paired host has a shelf.
    val hosts = remember { store.all().filter { it.paired && it.fpHex.isNotEmpty() } }
    val shelves = remember(hosts) {
        hosts.flatMap { host -> listOf(Shelf(host)) + presetStore.pinsFor(host).map { Shelf(host, it) } }
    }
    val last = remember { LibraryLayout.shelf(context) }
    val wanted = shelfId ?: last
    val shelf = shelves.firstOrNull { it.id == wanted }
        ?: StartScreen.defaultHost(settings.defaultHost, hosts).host?.let { host -> shelves.firstOrNull { it.id == host.id } }
        ?: shelves.firstOrNull()
    if (shelf == null) {
        NoShelf(onShowHosts)
        return
    }

    // Which hosts answer, and what each has up: the Desktops row. Blocked UDP would only time out.
    var online by remember { mutableStateOf(emptySet<String>()) }
    var playing by remember { mutableStateOf(emptyMap<String, String>()) }
    val services = remember { HostServices(store, JniHostNative(context)) }
    val lifecycle = (context as? LifecycleOwner)?.lifecycle
    LaunchedEffect(hosts, lnpGranted) {
        if (!lnpGranted) return@LaunchedEffect
        val identity = withContext(Dispatchers.IO) { IdentityHolder.shared(context).await() }
        while (true) {
            // The laps wait while the app is away.
            lifecycle?.withStarted {}
            val up = withContext(Dispatchers.IO) { hosts.filter { services.answers(it, null, 1_500) } }
            online = up.map { it.id }.toSet()
            if (identity != null) {
                playing = withContext(Dispatchers.IO) {
                    up.mapNotNull { host ->
                        LibraryClient.fetchRunning(
                            host.address, host.effectiveMgmtPort, identity.certPem, identity.privateKeyPem,
                            host.fpHex, host.port,
                        ).firstOrNull { it.isUp }?.title?.let { host.id to it }
                    }.toMap()
                }
            }
            delay(PRESENCE_LAP_MS)
        }
    }

    // A new shelf is a new screen: its own fetch, scroll and remembered title.
    key(shelf.id) {
        LibraryScreen(
            host = shelf.host,
            settings = settings,
            onLaunched = onLaunched,
            pinnedPresetId = shelf.preset?.id,
            autoStream = autoStream,
            desktops = hosts.map { DesktopTile(it, it.id in online, playing[it.id]) },
            header = if (shelves.size > 1) {
                {
                    ShelfChips(shelves, shelf.id) { picked ->
                        // A pick opens the shelf at its top: the remembered title is for coming
                        // back from a stream, not for switching hosts.
                        LibraryPosition.forget(context, picked.host.id)
                        onShelf(picked.id)
                    }
                }
            } else {
                null
            },
        )
    }
}

/** The host filter: one chip per shelf with its OS mark, the current one selected. */
@Composable
private fun ShelfChips(shelves: List<Shelf>, current: String, onPick: (Shelf) -> Unit) {
    Strip {
        items(shelves, key = { it.id }) { shelf ->
            FilterChip(
                selected = shelf.id == current,
                onClick = { onPick(shelf) },
                label = { Text(shelf.title, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                leadingIcon = resolveOsIcon(shelf.host.os)?.let { mark ->
                    { Icon(mark, contentDescription = null, modifier = Modifier.size(16.dp)) }
                },
            )
        }
    }
}

/** No paired host: nothing to browse yet, and the way to fix that. */
@Composable
private fun NoShelf(onShowHosts: () -> Unit) {
    Column(
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(8.dp, Alignment.CenterVertically),
        modifier = Modifier.fillMaxSize().padding(horizontal = 32.dp),
    ) {
        Text(stringResource(R.string.lib_no_host_title), style = MaterialTheme.typography.titleMedium)
        Text(
            stringResource(R.string.lib_no_host_body),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
        Button(onClick = onShowHosts, modifier = Modifier.padding(top = 8.dp)) {
            Text(stringResource(R.string.lib_no_host_show_hosts))
        }
    }
}
