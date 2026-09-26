package io.unom.punktfunk

import android.content.Context
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.rememberLazyGridState
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.PageSize
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.DesktopWindows
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.boundsInWindow
import androidx.compose.ui.layout.onGloballyPositioned
import android.content.res.Configuration
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.zIndex
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeSource
import kotlinx.coroutines.launch
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import coil.ImageLoader
import io.unom.punktfunk.components.launcherIcon
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.LibraryClient
import io.unom.punktfunk.kit.library.LibraryResult
import io.unom.punktfunk.kit.library.LibraryCache
import io.unom.punktfunk.kit.library.RunningGame
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.IdentityStore
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.obtainIdentity
import io.unom.punktfunk.models.ActiveSession
import io.unom.punktfunk.models.LaunchHold
import kotlin.math.PI
import kotlin.math.absoluteValue
import kotlin.math.cos
import kotlin.math.sign
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext

// The host game-library browser — the Android mirror of the Apple client's LibraryView: ONE screen
// with two presentations of the same shelf, chosen the same way every other screen here chooses.
// The console (gamepad) one is a poster coverflow (centered cover flat + prominent, neighbours
// receding on a 3D Y-tilt), reached with Y from a saved host; the touch one is the poster GRID the
// Apple, GTK and Windows shells draw, reached from a host card's "Browse library…". Both fetch from
// the host's management API over mTLS, and everything data-shaped — the fetch, the states, the
// launch — is shared: only the arrangement differs, because only the input device does.

/**
 * Whether the shelf on screen is an observation or a memory — and, if a memory, whether anything is
 * still being done about it.
 *
 * Three states rather than a flag because the two cached ones want different words. "Waking the
 * host…" says a shelf is about to become current; "last known library" says it isn't going to.
 * Telling a player the first thing while nothing is happening is the kind of lie a progress
 * indicator tells, and it is worth one extra state to never tell it.
 */
private enum class Stale(val note: String?) {
    No(null),
    Waking("Last known library — waking the host…"),
    Offline("Last known library — the host didn't answer"),
}

private sealed class LibState {
    object Loading : LibState()

    /**
     * The shelf. Carries the client identity so a launch can dial the host over the same pinned
     * mTLS trust.
     *
     * [running] is keyed by library id and is HOST state, never catalog state — it arrives from
     * `/status` separately and later, and is deliberately absent from what [LibraryCache] writes,
     * so a shelf served from disk can never claim a game is up because it was up last time
     * anybody looked.
     */
    data class Ready(
        val games: List<GameEntry>,
        val loader: ImageLoader,
        val identity: ClientIdentity,
        val running: Map<String, RunningGame> = emptyMap(),
        val stale: Stale = Stale.No,
    ) : LibState() {
        /**
         * Display order: anything already running first, so getting back into it is the first
         * thing on screen rather than something to scroll for.
         *
         * Applied on top of the launchers-first grouping the fetch already did, never instead of
         * it — a launcher that is up still belongs with the launchers, which is what keeps design
         * D4's two sections from interleaving. `sortedBy` is stable, so the host's own title order
         * survives inside each of the four resulting bands.
         */
        val ordered: List<GameEntry>
            get() {
                // …and the desktop tile leads all of it, so streaming the host itself is one tap
                // rather than a menu — and a host with no plugins still has something to tap.
                // Model state only: it is never fetched and never cached.
                val all = listOf(desktopTile) + games
                if (running.isEmpty()) return all
                return all.sortedBy {
                    if (it.isDesktop) -1
                    else (if (it.isLauncher) 0 else 2) + (if (running[it.id] != null) 0 else 1)
                }
            }

        /** Names what the tap does, so it does not read "Desktop" while tapping it resumes. */
        private val desktopTile: GameEntry
            get() = GameEntry.desktop(
                running.values.firstOrNull()?.let { "Resume ${it.title}" } ?: "Desktop",
            )
    }

    data class Message(val text: String) : LibState() // unauthorized / empty / error
}

/**
 * How long to keep asking a host we have just sent a magic packet to. A cold box takes 20–60 s to
 * POST and start serving, so one attempt would almost always land on a machine that is still
 * booting — the same 90-second budget [WakeController] allows.
 */
private const val WAKE_ATTEMPTS = 12
private const val WAKE_RETRY_MS = 5_000L

/**
 * Re-send the magic packet every other attempt (≈ every 10 s). A single packet can be missed, and
 * some NICs only wake on a fresh one after dropping into a deeper sleep state — [WakeController]'s
 * rule, expressed in this loop's units.
 */
private const val WAKE_RESEND_EVERY = 2

@Composable
fun LibraryScreen(
    host: KnownHost,
    settings: Settings,
    onLaunched: (ActiveSession) -> Unit,
    onBack: () -> Unit,
    /**
     * The preset this shelf launches with, when it was opened from a PINNED host+preset card
     * (design §5.2a) rather than the host's own tile: a one-off, exactly like the card's plain
     * connect. Null = the host's tile, and the host's binding decides — the same rule
     * [PresetStore.resolveFor] applies to every other connect.
     */
    pinnedPresetId: String? = null,
    /**
     * Stream this host's desktop as soon as the shelf can dial (`start_in = stream`). One attempt
     * per app start: a refusal leaves the shelf on screen and nothing retries, and [onLaunched]
     * disarms the caller's flag, so a shelf composed again after a stream stays a shelf. Goes
     * through the same [launch] every tap does, so the auto-start and a tap on the Desktop tile
     * cannot drift.
     */
    autoStream: Boolean = false,
) {
    BackHandler(onBack = onBack)
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    // Bumped by the touch header's Reload — re-keys the fetch effect below without the screen
    // having to be popped and re-pushed to try a flaky host again.
    var reloadKey by remember { mutableIntStateOf(0) }
    var state by remember { mutableStateOf<LibState>(LibState.Loading) }
    // A launch (connect) in flight: shows an overlay + gates the pad so a second press can't dial twice.
    var launching by remember { mutableStateOf(false) }
    // The preset every launch off this shelf runs with, resolved ONCE per shelf by the same rule
    // the host-list connect uses: this card's pin as the one-off, else the host's binding, else the
    // globals. Resolved here rather than per launch so a preset edited mid-browse cannot make two
    // titles on one shelf stream differently.
    val preset = remember(host.id, pinnedPresetId) {
        PresetStore(context).resolveFor(host, pinnedPresetId)
    }
    val streamSettings = remember(settings, preset) { settings.effectiveFor(preset) }

    // Keyed on the mgmt port too: a discovery tick can learn it after this screen is composed, and
    // the fetch must redo itself against the real port rather than stay on a stale 47990 failure.
    //
    // Four things happen here and the ORDER is the point:
    //   1. the CACHED catalog goes up immediately, marked stale — a library is the screen a player
    //      uses to decide what to play, and an empty one while a sleeping box boots is the opposite
    //      of useful;
    //   2. a magic packet goes out, so the box warms while they are still choosing — waking used to
    //      be bound to CONNECTING, which is far too late to help;
    //   3. the live fetch runs, retrying across the boot window, and replaces the cached shelf;
    //   4. `/status` says which titles are already up, AFTER the catalog so a slow answer can never
    //      hold the titles back.
    //
    // A cached catalog also outranks a failure: if the host never answers, the titles on screen are
    // still the right ones to choose from, and replacing them with an error because a box is asleep
    // is precisely what the cache exists to prevent.
    LaunchedEffect(host.address, host.port, host.fpHex, host.effectiveMgmtPort, reloadKey) {
        loadLibrary(context, host) { state = it }
    }

    // A pinned card's shelf says so, in the card's own `host · preset` shape: what a launch here
    // will use is a property of the shelf, not something to remember from the tile two screens back.
    val title = if (pinnedPresetId != null && preset != null) {
        "${host.name} · ${preset.name} — Library"
    } else {
        "${host.name} — Library"
    }

    // Dial the host over the same pinned mTLS trust, booting straight into this title (the host
    // resolves `launch` = its library id). Shared by both presentations: a tap on a grid tile and A
    // on a centred cover are the same act, and a launch that behaved differently between them would
    // be a bug nobody could see until they switched input device.
    fun launch(identity: ClientIdentity, game: GameEntry) {
        if (launching) return
        launching = true
        // The desktop tile is the host, not one of its titles: it streams with no launch id, and
        // there is no place to remember for something that is not in the catalog. Asking a host
        // to launch what it is already showing is how a second copy starts.
        if (!game.isDesktop) {
            // The player's place in this shelf, remembered as the TITLE rather than an index or a
            // scroll offset — the only moment they are definitely leaving the grid for one.
            // Recorded before the dial rather than after it succeeds: a failed launch still means
            // "this is the one I was going for", and coming back to it is right either way.
            LibraryPosition.remember(context, host.id, game.id)
        }
        scope.launch {
            val handle = connectToHost(
                context, streamSettings, identity,
                host.address, host.port, host.fpHex,
                launch = game.id.takeUnless { game.isDesktop },
                dialer = "touch/library",
                preset = preset,
            )
            launching = false
            if (handle != 0L) {
                onLaunched(
                    ActiveSession(
                        handle,
                        streamSettings,
                        host.clipboardSync,
                        presetName = preset?.name,
                        hostId = host.id,
                        // Where to come back to when this game exits — this shelf, pin and all,
                        // not the host's default one.
                        launchedFromLibrary = true,
                        libraryPresetId = pinnedPresetId,
                        // The host never tracks a launcher tile or the desktop, so there is
                        // nothing to wait for.
                        launchHold = game.takeUnless { it.isLauncher || it.isDesktop }?.let {
                            LaunchHold(
                                it, host.address, host.effectiveMgmtPort, host.fpHex,
                                sourceRect = TileFrames.rect(it.id),
                            )
                        },
                    ),
                )
            } else {
                Toast.makeText(
                    context,
                    "Launch failed — check the host and try again.",
                    Toast.LENGTH_LONG,
                ).show()
            }
        }
    }

    // `start_in = stream`: the shelf is on screen, so dial the desktop over it. Keyed on the
    // identity rather than `Unit` because the dial needs one, and it arrives with the fetch —
    // `launching` guards the second pass a state change would otherwise cause.
    val ready = (state as? LibState.Ready)?.identity
    LaunchedEffect(autoStream, ready) {
        if (autoStream && ready != null) launch(ready, GameEntry.desktop())
    }

    // "Copy link" for one TITLE — the self-emitted form a host card already hands out (design/
    // client-deep-links.md §4/§5), plus this game's `launch=` id, so pasting the URL into Playnite
    // or a Stream Deck macro boots straight into it. A shelf opened from a PINNED card copies that
    // card's preset with it, because that combination is the thing being copied.
    fun copyLink(game: GameEntry) {
        val url = DeepLinks.forHost(host, launch = game.id, preset = pinnedPresetId).toUrl()
        // A toast either way here: this screen renders neither the touch home's notice banner nor
        // the console's status line, and both of its presentations are full-bleed over artwork.
        linkCopyMessage(putLinkOnClipboard(context, url))?.let {
            Toast.makeText(context, it, Toast.LENGTH_SHORT).show()
        }
    }

    // Lambdas, NOT `::launch` / `::copyLink`: two callable references to the same local function
    // compare EQUAL however different the frame they captured, so a skipped recomposition would
    // leave the child calling a closure over stale settings. SettingsScreen documents the same
    // trap at `scopePreset()`, having been bitten by it.
    // Where this shelf should open. Read ONCE per screen (not per recomposition) so a launch
    // rewriting it mid-browse cannot make the grid jump under the player's thumb; the effects that
    // consume it are one-shot on top of that.
    val resumeAt = remember(host.id) { LibraryPosition.last(context, host.id) }

    TouchLibrary(
        title = title,
        state = state,
        launching = launching,
        onBack = onBack,
        onReload = { reloadKey++ },
        onLaunch = { identity, game -> launch(identity, game) },
        onCopyLink = { game -> copyLink(game) },
        resumeAt = resumeAt,
    )
}

/**
 * Fill the shelf. Four things happen and the ORDER is the point:
 *   1. the CACHED catalog goes up immediately, marked stale — a library is the screen a player
 *      uses to decide what to play, and an empty one while a sleeping box boots is the opposite
 *      of useful;
 *   2. a magic packet goes out, so the box warms while they are still choosing;
 *   3. the live fetch runs, retrying across the boot window, and replaces the cached shelf;
 *   4. `/status` says which titles are already up, AFTER the catalog so a slow answer can never
 *      hold the titles back.
 * A cached catalog also outranks a failure: if the host never answers, the titles on screen are
 * still the right ones to choose from.
 */
private suspend fun loadLibrary(context: Context, host: KnownHost, set: (LibState) -> Unit) {
    set(LibState.Loading)
    val cache = LibraryCache.standard(context.cacheDir)
    val (identity, loader) = prepareLoader(context, host) ?: run {
        set(LibState.Message("Identity unavailable — re-pair may be required"))
        return
    }
    // Keyed on the host RECORD id, not its address: a box that came back on a new DHCP lease is
    // the same host with the same library, and keying on where it lives would lose the cache
    // exactly when a cold-booted machine needs it most.
    val cached = withContext(Dispatchers.IO) { cache.load(host.id)?.games }?.takeIf { it.isNotEmpty() }
    if (cached != null) {
        set(LibState.Ready(cached, loader, identity, stale = Stale.Waking))
    }
    when (val res = fetchAcrossWake(host, identity)) {
        is LibraryResult.Ok -> if (res.games.isEmpty()) {
            set(LibState.Message("No games found on this host."))
        } else {
            set(LibState.Ready(res.games, loader, identity))
            // Remembered AFTER it is on screen: the disk write is not on the path to a shelf.
            withContext(Dispatchers.IO) { cache.store(host.id, res.games) }
            val running = withContext(Dispatchers.IO) {
                LibraryClient.fetchRunning(
                    address = host.address,
                    mgmtPort = host.effectiveMgmtPort,
                    certPem = identity.certPem,
                    keyPem = identity.privateKeyPem,
                    fpHex = host.fpHex,
                )
            }
                .filter { it.isUp }
                // Two sessions can have the same title up (the host admits concurrent
                // sessions); for a Resume badge either one is the same answer.
                .mapNotNull { g -> g.appId?.let { it to g } }
                .toMap()
            set(LibState.Ready(res.games, loader, identity, running = running))
        }
        // The shelf stays if we have one; only the words change. The player can still pick a
        // title — the launch dials and wakes the host on its own.
        else -> set(
            if (cached != null) {
                LibState.Ready(cached, loader, identity, stale = Stale.Offline)
            } else {
                // The fetch reports a phrase, not a sentence — this screen has no separate
                // title to carry the frame, so it supplies one (SkiaConsole passes its own).
                LibState.Message(
                    when (res) {
                        is LibraryResult.Unauthorized -> "Couldn't load the library — ${res.message}"
                        is LibraryResult.Error -> "Couldn't load the library — ${res.message}"
                        else -> "Couldn't load the library"
                    },
                )
            },
        )
    }
}

/**
 * The identity and the art loader, needed whether or not the host ever answers — cached posters
 * have to render behind a cached catalog with the box still down. Null = no identity.
 */
private suspend fun prepareLoader(context: Context, host: KnownHost): Pair<ClientIdentity, ImageLoader>? =
    withContext(Dispatchers.IO) {
        val id = runCatching { obtainIdentity(IdentityStore(context)) }.getOrNull() ?: return@withContext null
        val loader = runCatching { posterLoader(context, id, host.address, host.fpHex) }.getOrNull()
            ?: return@withContext null
        id to loader
    }

/**
 * The catalog fetch, with a magic packet ahead of it and resends across the boot window. The
 * packet is deliberately unconditional rather than only when the host looks offline: it is one
 * datagram an already-awake machine ignores, so finding out whether it is needed costs more than
 * sending it. Anything other than "can't reach it" is settled — see `LibraryResult.isTransient`.
 */
private suspend fun fetchAcrossWake(host: KnownHost, identity: ClientIdentity): LibraryResult? {
    val macs = host.mac.joinToString(",")
    val waking = host.mac.isNotEmpty()
    if (waking) {
        withContext(Dispatchers.IO) { NativeBridge.nativeWakeOnLan(macs, host.address) }
    }
    val attempts = if (waking) WAKE_ATTEMPTS else 1
    var last: LibraryResult? = null
    for (attempt in 0 until attempts) {
        val res = withContext(Dispatchers.IO) {
            LibraryClient.fetch(
                address = host.address,
                mgmtPort = host.effectiveMgmtPort,
                certPem = identity.certPem,
                keyPem = identity.privateKeyPem,
                fpHex = host.fpHex,
            )
        }
        last = res
        if (res is LibraryResult.Ok || !res.isTransient || attempt + 1 >= attempts) break
        if (attempt % WAKE_RESEND_EVERY == WAKE_RESEND_EVERY - 1) {
            withContext(Dispatchers.IO) { NativeBridge.nativeWakeOnLan(macs, host.address) }
        }
        delay(WAKE_RETRY_MS)
    }
    return last
}

/**
 * The touch shelf: a Material poster grid under a back/reload header — the same page the Apple,
 * GTK and Windows shells draw, and the reason a finger-driven user can reach the library at all.
 *
 * Deliberately NOT the coverflow with the pad legend hidden: a coverflow is a one-at-a-time strip
 * built for a D-pad, and on a phone it turns a 400-title library into 400 swipes. The grid is what
 * every other touch surface in this app (and every other client's) already shows.
 */
@Composable
private fun TouchLibrary(
    title: String,
    state: LibState,
    launching: Boolean,
    onBack: () -> Unit,
    onReload: () -> Unit,
    onLaunch: (ClientIdentity, GameEntry) -> Unit,
    onCopyLink: (GameEntry) -> Unit,
    /** The title this shelf last launched — where the grid opens. Null on a first visit. */
    resumeAt: String? = null,
) {
    Box(Modifier.fillMaxSize()) {
        Column(Modifier.fillMaxSize().safeArea()) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.fillMaxWidth().padding(start = 4.dp, end = 4.dp, top = 8.dp),
            ) {
                IconButton(onClick = onBack) {
                    Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                }
                Text(
                    title,
                    style = MaterialTheme.typography.titleLarge,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                // A shelf that failed to load is otherwise a dead end you have to back out of and
                // re-enter; every other client's library page has had this from the start.
                IconButton(onClick = onReload, enabled = state !is LibState.Loading) {
                    Icon(Icons.Filled.Refresh, contentDescription = "Reload")
                }
            }
            // Says the titles below are remembered rather than observed — shown only while that is
            // true, and never as an error: a cached library is a working library, and a host that
            // is still waking is the case this whole path exists to serve.
            (state as? LibState.Ready)?.stale?.note?.let {
                Text(
                    it,
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier
                        .semantics { liveRegion = LiveRegionMode.Polite }
                        .padding(horizontal = 20.dp, vertical = 2.dp),
                )
            }
            when (state) {
                is LibState.Loading -> Box(
                    Modifier.weight(1f).fillMaxWidth(),
                    contentAlignment = Alignment.Center,
                ) {
                    Column(
                        horizontalAlignment = Alignment.CenterHorizontally,
                        verticalArrangement = Arrangement.spacedBy(14.dp),
                    ) {
                        CircularProgressIndicator()
                        Text("Loading library…", style = MaterialTheme.typography.bodyLarge)
                    }
                }
                is LibState.Message -> Box(
                    Modifier.weight(1f).fillMaxWidth(),
                    contentAlignment = Alignment.Center,
                ) {
                    Text(
                        state.text,
                        style = MaterialTheme.typography.bodyLarge,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        textAlign = TextAlign.Center,
                        modifier = Modifier.padding(horizontal = 24.dp),
                    )
                }
                is LibState.Ready -> TouchGrid(
                    games = state.ordered,
                    loader = state.loader,
                    onLaunch = { game -> onLaunch(state.identity, game) },
                    onCopyLink = onCopyLink,
                    running = state.running,
                    resumeAt = resumeAt,
                    modifier = Modifier.weight(1f),
                )
            }
        }
        if (launching) {
            Box(
                Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.55f)),
                contentAlignment = Alignment.Center,
            ) {
                Column(
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(14.dp),
                ) {
                    CircularProgressIndicator(color = Color.White)
                    Text("Launching…", color = Color.White, style = MaterialTheme.typography.bodyLarge)
                }
            }
        }
    }
}

/**
 * The poster grid. Design D4: launcher entries get their own shelf above the titles, never
 * interleaved, and the headings only appear when both groups exist — so a library without
 * launchers looks exactly like a plain grid.
 *
 * Internal (not private) for the same reason as [Coverflow]: the screenshot harness composes the
 * real grid with a mock shelf, because the screen around it takes its state off the network.
 */
@Composable
internal fun TouchGrid(
    games: List<GameEntry>,
    loader: ImageLoader,
    onLaunch: (GameEntry) -> Unit,
    onCopyLink: (GameEntry) -> Unit,
    /**
     * Which titles the host already has up, keyed by library id — so a tile the player can return
     * to says `Resume` rather than looking like every other one. Empty on an older host, an
     * unreachable one, and while a shelf is being served from cache.
     */
    running: Map<String, RunningGame> = emptyMap(),
    /** The title this shelf last launched — where the grid opens. Null on a first visit. */
    resumeAt: String? = null,
    modifier: Modifier = Modifier,
) {
    val launchers = games.filter { it.isLauncher }
    val titles = games.filter { !it.isLauncher }
    val both = launchers.isNotEmpty() && titles.isNotEmpty()
    val grid = rememberLazyGridState()
    // Put the player back where they were. Leaving a stream re-composes this screen from scratch —
    // a new LazyGridState — so a long library came back at the top every time, and the round trip
    // this shelf exists for (browse → play → quit → browse) lost your place on every lap.
    //
    // Anchored on the TITLE, not an index or a pixel offset: an offset is meaningless across the
    // things that legitimately change between visits (a rotation, a resize, a foldable unfolding,
    // a host that gained titles, the running-first ordering above), and the list is re-ordered at
    // least twice per visit as the cached catalog gives way to the live one and then to `/status`.
    // Keyed on the list so it can still land once the title it names arrives, and one-shot so a
    // later re-order never yanks the grid out from under someone who has started scrolling.
    var restored by rememberSaveable { mutableStateOf(false) }
    LaunchedEffect(games, resumeAt) {
        if (restored || resumeAt == null) return@LaunchedEffect
        val index = flatIndexOf(resumeAt, launchers, titles, both)
        if (index == null) return@LaunchedEffect
        restored = true
        // Without animation: it should simply already be there, not visibly travel.
        grid.scrollToItem(index)
    }
    LazyVerticalGrid(
        columns = GridCells.Adaptive(minSize = 130.dp),
        state = grid,
        modifier = modifier.fillMaxWidth(),
        contentPadding = PaddingValues(16.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        if (launchers.isNotEmpty()) {
            if (both) item(span = { GridItemSpan(maxLineSpan) }) { TouchGroupHeading("Launchers") }
            items(launchers, key = { "launcher-${it.id}" }) {
                TouchPoster(it, loader, onLaunch, onCopyLink, running[it.id] != null)
            }
        }
        if (titles.isNotEmpty()) {
            if (both) item(span = { GridItemSpan(maxLineSpan) }) { TouchGroupHeading("Games") }
            items(titles, key = { "game-${it.id}" }) {
                TouchPoster(it, loader, onLaunch, onCopyLink, running[it.id] != null)
            }
        }
    }
}

/**
 * Where a title sits in the FLAT item stream `LazyVerticalGrid` scrolls by, headings included.
 *
 * The grid is not one list: design D4 gives the launchers their own section, and each section
 * carries a full-span heading whenever both exist. `scrollToItem` counts those headings, so an
 * index taken from `games` alone lands one or two tiles short — which is exactly the kind of
 * off-by-a-heading that only shows up on a library that happens to have launchers.
 *
 * Null when the title isn't in this grid at all: a host that lost it since, or a shelf filtered
 * down to a group it doesn't belong to.
 */
private fun flatIndexOf(
    id: String,
    launchers: List<GameEntry>,
    titles: List<GameEntry>,
    both: Boolean,
): Int? {
    val heading = if (both) 1 else 0
    launchers.indexOfFirst { it.id == id }.takeIf { it >= 0 }?.let { return heading + it }
    val gamesStart = if (launchers.isEmpty()) 0 else heading + launchers.size
    return titles.indexOfFirst { it.id == id }.takeIf { it >= 0 }?.let { gamesStart + heading + it }
}

@Composable
private fun TouchGroupHeading(text: String) {
    Text(
        text.uppercase(),
        style = MaterialTheme.typography.labelSmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        letterSpacing = 1.4.sp,
    )
}

/**
 * One touch tile: 2:3 poster, store badge, title. Tap launches; a LONG PRESS opens this title's own
 * actions — the finger's context menu, and the same gesture the host cards' overflow answers to.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun TouchPoster(
    game: GameEntry,
    loader: ImageLoader,
    onLaunch: (GameEntry) -> Unit,
    onCopyLink: (GameEntry) -> Unit,
    /** Already up on the host, so tapping resumes rather than starts. */
    running: Boolean = false,
) {
    var menu by remember { mutableStateOf(false) }
    val shape = MaterialTheme.shapes.medium
    Box {
        Column(
            Modifier.combinedClickable(
                onClickLabel = "Launch ${game.title}",
                onLongClickLabel = "More options for ${game.title}",
                onClick = { onLaunch(game) },
                onLongClick = { menu = true },
            ),
        ) {
            Box(
                Modifier
                    .fillMaxWidth()
                    .aspectRatio(2f / 3f)
                    // What the launch hold flies this title's cover out of.
                    .onGloballyPositioned { TileFrames.record(game.id, it.boundsInWindow()) }
                    .clip(shape)
                    .background(MaterialTheme.colorScheme.surfaceVariant),
                contentAlignment = Alignment.Center,
            ) {
                TouchPosterArt(game, loader)
                // Opposite corner from the store chip, so the two never meet on a narrow tile.
                if (running) {
                    Box(Modifier.fillMaxSize().padding(6.dp), contentAlignment = Alignment.TopEnd) {
                        RunningBadge(compact = true)
                    }
                }
                Box(Modifier.fillMaxSize().padding(6.dp), contentAlignment = Alignment.TopStart) {
                    Text(
                        game.storeLabel,
                        style = MaterialTheme.typography.labelSmall,
                        // A launcher's badge is brand-filled (design D4); a game's sits on a dark
                        // wash over its own art, where the theme's own ink would be a coin toss.
                        color = if (game.isLauncher) {
                            MaterialTheme.colorScheme.onPrimary
                        } else {
                            Color.White
                        },
                        modifier = Modifier
                            .semantics {
                                contentDescription = if (game.isLauncher) {
                                    "Opens ${game.storeLabel}"
                                } else {
                                    "From ${game.storeLabel}"
                                }
                            }
                            .clip(PillShape)
                            .background(
                                if (game.isLauncher) {
                                    MaterialTheme.colorScheme.primary
                                } else {
                                    Color.Black.copy(alpha = 0.5f)
                                },
                            )
                            .padding(horizontal = 8.dp, vertical = 3.dp),
                    )
                }
            }
            Text(
                game.title,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.padding(top = 6.dp),
            )
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(
                text = { Text("Copy link") },
                onClick = {
                    menu = false
                    onCopyLink(game)
                },
            )
        }
    }
}

/**
 * "This one is already up on the host" — the Resume affordance, overlaid on a poster.
 *
 * A badge rather than a changed action label because these tiles have no labels to change: the
 * poster *is* the control. It says `Resume` rather than `Running` on purpose — the player does not
 * need a status report, they need to know what tapping it will do.
 *
 * A fixed semantic green rather than the theme's primary: this is a state the HOST reports, not a
 * Punktfunk surface, and it has to stay distinguishable from the launcher chip, which already owns
 * the brand fill one corner away. Fixed also means it survives Material You seeding the touch theme
 * from a wallpaper that happens to be green.
 */
@Composable
private fun TouchPosterArt(game: GameEntry, loader: ImageLoader) {
    PosterArt(game, loader) {
        // A launcher ships no poster by design, so its brand mark IS the poster; falling back to the
        // launcher's NAME says "opens Steam", where a title would read as "a cover that failed to load".
        // The desktop tile is not a brand, so its mark comes from Material rather than the brand
        // table — the same job the Lucide `monitor` does on the shells that carry that set.
        val mark = launcherIcon(game.iconToken)
            ?: Icons.Filled.DesktopWindows.takeIf { game.isDesktop }
        if (mark != null) {
            Icon(
                imageVector = mark,
                contentDescription = game.title,
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.fillMaxSize(0.45f),
            )
        } else {
            Text(
                if (game.isLauncher) game.storeLabel else game.title,
                style = MaterialTheme.typography.titleSmall,
                fontWeight = FontWeight.SemiBold,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(12.dp),
            )
        }
    }
}

@Composable
private fun RunningBadge(compact: Boolean = false) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .semantics { contentDescription = "Running on the host \u2014 resume" }
            .clip(PillShape)
            .background(RESUME_GREEN)
            .padding(horizontal = if (compact) 6.dp else 8.dp, vertical = 3.dp),
    ) {
        Icon(
            Icons.Filled.PlayArrow,
            contentDescription = null,
            tint = RESUME_INK,
            modifier = Modifier.size(12.dp),
        )
        // Glyph only on the touch grid, whose tiles go down to ~130 dp wide and already carry the
        // store chip in the opposite corner; at that size "Resume" plus an icon leaves the two
        // badges touching in the middle. The coverflow's covers are several times wider and take
        // the word.
        if (!compact) {
            Text(
                "Resume",
                style = MaterialTheme.typography.labelSmall,
                fontWeight = FontWeight.SemiBold,
                color = RESUME_INK,
                modifier = Modifier.padding(start = 4.dp),
            )
        }
    }
}

/** The host presence green the console palette already uses for "this machine is up". */
private val RESUME_GREEN = Color(0xFF33D64A)

/** Near-black on that green — a fixed pair, so neither theme nor palette can wash it out. */
private val RESUME_INK = Color(0xFF0A1A0D)

