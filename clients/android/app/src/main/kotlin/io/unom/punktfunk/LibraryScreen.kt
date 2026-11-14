package io.unom.punktfunk

import android.content.Context
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.annotation.StringRes
import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import coil3.ImageLoader
import io.unom.punktfunk.kit.ListedProfile
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.library.Download
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.Hide
import io.unom.punktfunk.kit.library.HideOutcome
import io.unom.punktfunk.kit.library.HostStatus
import io.unom.punktfunk.kit.library.InstallAction
import io.unom.punktfunk.kit.library.InstallOutcome
import io.unom.punktfunk.kit.library.LibraryCache
import io.unom.punktfunk.kit.library.LibraryClient
import io.unom.punktfunk.kit.library.LibraryResult
import io.unom.punktfunk.kit.library.RunningGame
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.say
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.IdentityHolder
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.models.ActiveSession
import io.unom.punktfunk.models.LaunchHold
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

// One shelf of the touch library: a host's catalog fetched over its management API on the paired
// identity, the acts on its titles, and the dial a launch makes. [TouchLibrary] draws it. The
// console shell has its own shelf and shares the fetch, the cache and the launch.

/**
 * Whether the shelf on screen is an observation or a memory — and, if a memory, what is being done
 * about it. The cached states want different words: "waking" only while a magic packet is out,
 * "checking" while the host is merely asked, and "last known library" once it isn't going to
 * become current. The same copy as `pf_console_ui::Stale`.
 */
internal enum class Stale(@StringRes val note: Int?) {
    No(null),
    Checking(R.string.lib_stale_checking),
    Waking(R.string.lib_stale_waking),
    Offline(R.string.lib_stale_offline),
}

internal sealed class LibState {
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
        /** `/status` downloads by library id, and this device's grants: host state, as [running]. */
        val downloads: Map<String, Download> = emptyMap(),
        val grants: Int? = null,
    ) : LibState()

    data class Message(val text: Say) : LibState() // unauthorized / empty / error
}

/** A `/status` read on the shelf: what is up, what downloads, and what this device may do. */
private fun LibState.Ready.withStatus(status: HostStatus) = copy(
    running = status.games
        .filter { it.isUp }
        // Two sessions can have the same title up (the host admits concurrent sessions); for a
        // Running badge either one is the same answer, and the endable one (sorted last, so it
        // wins) carries End game.
        .sortedBy { it.endable }
        .mapNotNull { g -> g.appId?.let { it to g } }
        .toMap(),
    downloads = status.downloads.associateBy { it.appId },
    grants = status.grants,
)

/**
 * One shelf of the Library tab: [host]'s catalog, and every act on its titles. [pinnedPresetId] is
 * set for a pinned host+preset card's shelf: each launch off it runs with that preset, as the
 * card's own connect does. Null follows the host's binding ([PresetStore.resolveFor]).
 */
@Composable
internal fun LibraryScreen(
    host: KnownHost,
    settings: Settings,
    onLaunched: (ActiveSession) -> Unit,
    pinnedPresetId: String? = null,
    /**
     * Stream this host's desktop as soon as the shelf can dial (`start_in = stream`). One attempt
     * per app start: a refusal leaves the shelf on screen, and [onLaunched] disarms the caller's
     * flag. It goes through the same [launch] every tap does.
     */
    autoStream: Boolean = false,
    /** Every paired host's desktop, for the Desktops row. */
    desktops: List<DesktopTile> = emptyList(),
    /** Drawn over the sections: the shelf chips. */
    header: (@Composable () -> Unit)? = null,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    // Bumped by Reload: re-keys the fetch below.
    var reloadKey by remember { mutableIntStateOf(0) }
    var state by remember { mutableStateOf<LibState>(LibState.Loading) }
    // A launch in flight: shows an overlay, and a second press can't dial twice.
    var launching by remember { mutableStateOf(false) }
    val presetStore = remember { PresetStore(context) }
    // How this device arranges a shelf, and the titles it marked on this host.
    var layout by remember { mutableStateOf(LibraryLayout.load(context)) }
    var favorites by remember(host.fpHex) { mutableStateOf(LibraryLayout.favorites(context, host.fpHex)) }
    val knownHostStore = remember { KnownHostStore(context) }
    val services = remember { HostServices(knownHostStore, JniHostNative(context)) }
    // The profile picker a launch waits on.
    var profileAsk by remember { mutableStateOf<ProfileAsk?>(null) }
    profileAsk?.let { ask ->
        ProfilePickerDialog(
            hostName = ask.host.name.ifBlank { ask.host.address },
            answer = ProfilesAnswer.Listed(ask.listed),
            saved = ask.host.asProfile,
            gone = ask.gone,
            art = rememberProfileArt(ask.host),
            onPick = { ask.answer.complete(it) },
            onDismiss = { ask.answer.complete(null) },
        )
    }
    var seatWait by remember { mutableStateOf<ProfileWait?>(null) }
    seatWait?.let { w ->
        SeatWaitDialog(w, onCancel = { w.cancelled.complete(Unit); seatWait = null })
    }
    // The password prompt a launch waits on.
    var unlockAsk by remember { mutableStateOf<PasswordAsk?>(null) }
    unlockAsk?.let { UnlockPromptDialog(it) }

    // Hide for me ([Hide]): the shelf's profile row while the box lets this device hide its titles,
    // and what that profile hides, shown in the shelf's place while asked for (null: the shelf).
    var hides by remember { mutableStateOf<ListedProfile?>(null) }
    var hiddenView by remember { mutableStateOf<LibState?>(null) }
    var hiddenReload by remember { mutableIntStateOf(0) }
    // The title that takes focus once the one beside it left the grid.
    var refocus by remember { mutableStateOf<String?>(null) }
    val snackbar = remember { SnackbarHostState() }
    BackHandler(enabled = hiddenView != null) { hiddenView = null }

    // Keyed on the mgmt port too: a discovery tick can learn it after this screen is composed, and
    // the fetch must redo itself against the real port rather than stay on a stale 47990 failure.
    // And on the saved pick: the shelf is the library that profile plays, and only it hides.
    LaunchedEffect(host.address, host.port, host.fpHex, host.effectiveMgmtPort, host.asProfile?.id, reloadKey) {
        val offer = { row: ListedProfile? ->
            hides = row
            if (row == null) hiddenView = null
        }
        if (hides?.id != host.asProfile?.id) offer(null)
        loadLibrary(context, host, settings.autoWakeEnabled, offer) { state = it }
    }
    val hiddenOpen = hiddenView != null
    LaunchedEffect(hiddenOpen, hiddenReload) {
        val row = hides
        if (!hiddenOpen || row == null) return@LaunchedEffect
        hiddenView = LibState.Loading
        hiddenView = loadHidden(context, host, row.id, state)
    }

    // Dial [target] on the pinned trust and the profile's unlock, booting straight into [game]
    // with its preset and the host's overrides for it. The desktop entry streams with no launch
    // id: asking a host to launch what it already shows starts a second copy. [target] is this
    // shelf's host, or another paired host for its desktop tile, which takes no pinned preset.
    // [link] names the profile to play as; [redial] marks the one dial [ProfileRetry] grants.
    fun launch(
        identity: ClientIdentity,
        game: GameEntry,
        link: String? = null,
        redial: Boolean = false,
        target: KnownHost = host,
    ) {
        if (launching) return
        launching = true
        val pin = pinnedPresetId.takeIf { target.id == host.id }
        if (!game.isDesktop) {
            // The player's place in this shelf, as the title. Before the dial: a failed launch
            // still names the one they were going for.
            LibraryPosition.remember(context, target.id, game.id)
        }
        scope.launch {
            val choice = chooseProfile(knownHostStore, identity, target, link, wait = { seatWait = it }) {
                profileAsk = it
            }
            profileAsk = null
            if (choice !is ProfileChoice.Dial) {
                launching = false
                if (choice is ProfileChoice.Refused) {
                    Toast.makeText(context, choice.line.words(context.resources), Toast.LENGTH_LONG).show()
                }
                return@launch
            }
            val launchId = game.id.takeUnless { game.isDesktop }
            val launchPreset = presetStore.resolveFor(target, pin, launchId)
            val spec = DialSpec(
                settings.effectiveFor(launchPreset), identity, target.address, target.port, target.fpHex,
                launch = launchId,
                dialer = "touch/library",
                preset = launchPreset,
                profile = choice.id,
                promptsUnlock = promptsUnlock(context),
                overrides = game.overrides,
            )
            val hooks = touchUnlockHooks(context, identity, target, ask = { ask ->
                unlockAsk = ask
                try {
                    ask.answer.await()
                } finally {
                    unlockAsk = null
                }
            })
            val dialed = dialUnlocked(hooks, spec.pinHex, choice.row) { unlock ->
                services.dial(spec, target, target.name, unlock = unlock)
            }
            launching = false
            if (dialed is HostServices.Connected) {
                onLaunched(
                    dialed.session.copy(
                        // Where to come back to when this game exits: this shelf, pin and all.
                        launchedFromLibrary = true,
                        libraryPresetId = pin,
                        // The host never tracks a launcher tile or the desktop, so there is
                        // nothing to wait for.
                        launchHold = game.takeUnless { it.isLauncher || it.isDesktop }?.let {
                            LaunchHold(
                                it, target.address, target.effectiveMgmtPort, target.fpHex,
                                sourceRect = TileFrames.rect(it.id), streamPort = target.port,
                            )
                        },
                    ),
                )
            } else if (dialed is HostServices.Failed) {
                val token = dialed.token
                if (ProfileRetry.afterRefusal(token, target, choice, redial, knownHostStore) { h, p ->
                        stillListed(identity, h, p)
                    }
                ) {
                    launch(identity, game, choice.id, redial = true, target = target)
                    return@launch
                }
                Toast.makeText(
                    context,
                    dialed.said.ifEmpty {
                        context.getString(ConnectErrors.connectMessage(token, requestAccess = false))
                    },
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
        if (autoStream && ready != null) launch(ready, GameEntry.desktop(context.getString(R.string.lib_desktop)))
    }

    // A desktop tile: stream that host, launching nothing. The identity is the device's, so a
    // shelf that failed to load still streams a desktop.
    fun streamDesktop(target: KnownHost) {
        scope.launch {
            val identity = (state as? LibState.Ready)?.identity
                ?: withContext(Dispatchers.IO) { IdentityHolder.shared(context).await() }
                ?: return@launch
            launch(identity, GameEntry.desktop(context.getString(R.string.lib_desktop)), target = target)
        }
    }

    // One TITLE's link handed out (design/client-deep-links.md §4/§5): the host card's form plus
    // this game's `launch=` id, so Playnite, a frontend or a home-screen tap boots straight into
    // it. A shelf opened from a PINNED card carries that card's preset: that combination is the
    // thing handed out. Every answer is a toast: this screen has no notice banner or status line.
    fun linkAction(game: GameEntry, action: LinkAction) {
        val url = DeepLinks.forHost(host, launch = game.id, preset = pinnedPresetId).toUrl()
        val name = if (game.isDesktop) host.name else game.title
        val toast = { text: String -> Toast.makeText(context, text, Toast.LENGTH_SHORT).show() }
        when (action) {
            LinkAction.Copy -> linkCopyMessage(putLinkOnClipboard(context, url))?.let { toast(context.getString(it)) }
            LinkAction.LauncherFile -> (context as? MainActivity)?.saveLauncherFile(name, url, toast)
            LinkAction.HomeScreen -> {
                val identity = (state as? LibState.Ready)?.identity
                val key = shortcutId(host.id, game.id.takeUnless { game.isDesktop }, pinnedPresetId)
                scope.launch {
                    withContext(Dispatchers.IO) {
                        pinTitleShortcut(context, identity, host, game.art.posterCandidates, url, name, key)
                    }?.let(toast)
                }
            }
        }
    }

    // Lambdas, NOT `::launch` / `::linkAction`: two callable references to the same local function
    // compare EQUAL however different the frame they captured, so a skipped recomposition would
    // leave the child calling a closure over stale settings. SettingsScreen documents the same
    // trap at `scopePreset()`, having been bitten by it.
    // Where this shelf should open. Read ONCE per screen (not per recomposition) so a launch
    // rewriting it mid-browse cannot make the grid jump under the player's thumb; the effects that
    // consume it are one-shot on top of that.
    val resumeAt = remember(host.id) { LibraryPosition.last(context, host.id) }

    // End game: the title the confirm asks about, then the host's answer as a toast. Gone either
    // way drops the Running badge here; the next load reads the host again.
    var endAsk by remember { mutableStateOf<GameEntry?>(null) }
    fun endGame(game: GameEntry) {
        val ready = state as? LibState.Ready ?: return
        scope.launch {
            val outcome = withContext(Dispatchers.IO) {
                LibraryClient.endGame(
                    ready.identity, host.address, host.effectiveMgmtPort, host.fpHex, game.id, host.port,
                )
            }
            if (outcome.gameGone) {
                (state as? LibState.Ready)?.let { state = it.copy(running = it.running - game.id) }
            }
            Toast.makeText(context, outcome.notice(game.title).words(context.resources), Toast.LENGTH_SHORT).show()
        }
    }
    // A title's files: the action, the host's answer as a toast, then `/status` again. A removal
    // turns the tile to "not installed" here; the next load reads the catalog again.
    var removeAsk by remember { mutableStateOf<GameEntry?>(null) }
    fun refreshStatus() {
        val ready = state as? LibState.Ready ?: return
        scope.launch {
            val status = withContext(Dispatchers.IO) {
                LibraryClient.fetchStatus(
                    host.address, host.effectiveMgmtPort, ready.identity.certPem,
                    ready.identity.privateKeyPem, host.fpHex, host.port,
                )
            }
            (state as? LibState.Ready)?.let { state = it.withStatus(status) }
        }
    }
    fun changeInstall(game: GameEntry, action: InstallAction) {
        val ready = state as? LibState.Ready ?: return
        scope.launch {
            val outcome = withContext(Dispatchers.IO) {
                LibraryClient.changeInstall(
                    ready.identity, host.address, host.effectiveMgmtPort, host.fpHex, game.id, action,
                    host.port,
                )
            }
            if (outcome == InstallOutcome.Done && action == InstallAction.Remove) {
                (state as? LibState.Ready)?.let { s ->
                    state = s.copy(
                        games = s.games.map { g ->
                            if (g.id == game.id) g.copy(install = g.install?.copy(state = "missing")) else g
                        },
                    )
                }
            }
            Toast.makeText(
                context,
                outcome.notice(action, game.title).words(context.resources),
                Toast.LENGTH_SHORT,
            ).show()
            refreshStatus()
        }
    }
    // Hide ([hidden]) or show [game] for the shelf's profile. What the host takes moves at once,
    // with no refetch, the cached catalog follows, and [next] takes its focus. A hide offers Undo,
    // which puts the title back at [at]. A refusal is the host's sentence and changes nothing.
    val undo = stringResource(R.string.common_undo)
    fun setHidden(game: GameEntry, hidden: Boolean, next: String?, at: Int? = null) {
        val row = hides ?: return
        val ready = state as? LibState.Ready ?: hiddenView as? LibState.Ready ?: return
        scope.launch {
            val outcome = withContext(Dispatchers.IO) {
                Hide.set(
                    ready.identity, host.address, host.effectiveMgmtPort, host.fpHex, row.id, game.id, hidden, host.port,
                )
            }
            val notice = outcome.notice(hidden, game.title, row.displayName).words(context.resources)
            // The newest line replaces the one up, rather than queueing behind it.
            snackbar.currentSnackbarData?.dismiss()
            if (outcome != HideOutcome.Done) {
                snackbar.showSnackbar(notice)
                return@launch
            }
            var from: Int? = null
            when (val s = state) {
                is LibState.Ready -> state = if (hidden) {
                    from = s.games.indexOfFirst { it.id == game.id }
                    s.copy(games = s.games.filter { it.id != game.id })
                } else {
                    val rest = s.games.filter { it.id != game.id }
                    val i = (at ?: rest.size).coerceIn(0, rest.size)
                    s.copy(games = rest.take(i) + game + rest.drop(i))
                }
                // A shelf that was empty, or failed while the host now answers, holds this one.
                is LibState.Message -> if (!hidden) state = LibState.Ready(listOf(game), ready.loader, ready.identity)
                LibState.Loading -> {}
            }
            (hiddenView as? LibState.Ready)?.let { v -> hiddenView = v.copy(games = v.games.filter { it.id != game.id }) }
            refocus = next
            withContext(Dispatchers.IO) {
                LibraryCache.standard(context.cacheDir)
                    .noteHidden(LibraryCache.keyFor(host, host.fpHex), row.id, game.id, game.takeIf { !hidden })
            }
            val result = snackbar.showSnackbar(
                notice,
                actionLabel = undo.takeIf { hidden },
                duration = if (hidden) SnackbarDuration.Long else SnackbarDuration.Short,
            )
            if (result == SnackbarResult.ActionPerformed) setHidden(game, hidden = false, next = game.id, at = from)
        }
    }
    // Percentages move while a download runs and the shelf is up.
    val downloading = (state as? LibState.Ready)?.downloads?.values?.any { it.live } == true
    LaunchedEffect(downloading) {
        while (downloading) {
            delay(2_000)
            refreshStatus()
        }
    }
    removeAsk?.let { game ->
        PunktfunkDialog(
            title = stringResource(R.string.lib_remove_confirm_title, game.title),
            onDismiss = { removeAsk = null },
            actions = listOf(
                DialogAction(stringResource(R.string.lib_download_remove), primary = true) {
                    removeAsk = null
                    changeInstall(game, InstallAction.Remove)
                },
                DialogAction(stringResource(R.string.common_cancel)) { removeAsk = null },
            ),
        ) { PromptText(stringResource(R.string.lib_remove_confirm_body)) }
    }
    endAsk?.let { game ->
        PunktfunkDialog(
            title = stringResource(R.string.lib_end_game_confirm_title, game.title),
            onDismiss = { endAsk = null },
            actions = listOf(
                DialogAction(stringResource(R.string.lib_end_game), primary = true) {
                    endAsk = null
                    endGame(game)
                },
                DialogAction(stringResource(R.string.common_cancel)) { endAsk = null },
            ),
        ) { PromptText(stringResource(R.string.lib_end_game_confirm_body)) }
    }

    TouchLibrary(
        state = state,
        launching = launching,
        onReload = { if (hiddenOpen) hiddenReload++ else reloadKey++ },
        layout = layout,
        onLayout = {
            layout = it
            LibraryLayout.save(context, settings, it)
        },
        favorites = favorites,
        onFavorite = { game -> favorites = LibraryLayout.toggleFavorite(context, settings, host.fpHex, game.id) },
        desktops = desktops,
        onDesktop = { target -> streamDesktop(target) },
        header = header,
        hidesFor = hides?.displayName,
        hidden = hiddenView,
        onHidden = { open -> hiddenView = if (open) LibState.Loading else null },
        onSetHidden = { game, hidden, next -> setHidden(game, hidden, next) },
        refocus = refocus,
        onRefocused = { refocus = null },
        snackbar = snackbar,
        onLaunch = { identity, game -> launch(identity, game) },
        onLink = { game, action -> linkAction(game, action) },
        onEndGame = { game -> endAsk = game },
        onFiles = { game, action ->
            if (action == InstallAction.Remove) removeAsk = game else changeInstall(game, action)
        },
        resumeAt = resumeAt,
    )
}

/**
 * Fill the shelf. Four things happen and the ORDER is the point:
 *   1. the CACHED catalog goes up immediately, marked stale — a library is the screen a player
 *      uses to decide what to play, and an empty one while a sleeping box boots is the opposite
 *      of useful;
 *   2. with [autoWake] on, a magic packet goes out so the box warms while they are still choosing,
 *      and only then does the shelf say "waking";
 *   3. the live fetch runs, retrying across the boot window, and replaces the cached shelf;
 *   4. `/status` says which titles are already up, AFTER the catalog so a slow answer can never
 *      hold the titles back;
 *   5. last, the profile the shelf was fetched as goes to [offer] when its `enumerate` row lets
 *      this device hide its titles ([Hide.offer]), else null.
 * A cached catalog also outranks a failure: if the host never answers, the titles on screen are
 * still the right ones to choose from.
 */
private suspend fun loadLibrary(
    context: Context,
    host: KnownHost,
    autoWake: Boolean,
    offer: (ListedProfile?) -> Unit,
    set: (LibState) -> Unit,
) {
    set(LibState.Loading)
    val cache = LibraryCache.standard(context.cacheDir)
    val key = LibraryCache.keyFor(host, host.fpHex)
    val (identity, loader) = prepareLoader(context, host) ?: run {
        set(LibState.Message(say(R.string.android_lib_identity_unavailable)))
        return
    }
    val profile = host.asProfile?.id
    val cached = withContext(Dispatchers.IO) { cache.load(key, profile)?.games }?.takeIf { it.isNotEmpty() }
    if (cached != null) {
        set(LibState.Ready(cached, loader, identity, stale = Stale.Checking))
    }
    val res = withContext(Dispatchers.IO) {
        LibraryClient.fetchAcrossWake(
            address = host.address,
            mgmtPort = host.effectiveMgmtPort,
            certPem = identity.certPem,
            keyPem = identity.privateKeyPem,
            fpHex = host.fpHex,
            macs = host.mac,
            autoWake = autoWake,
            profile = profile,
            streamPort = host.port,
            isCancelled = { !isActive },
            onWaking = {
                if (cached != null) {
                    launch(Dispatchers.Main) {
                        set(LibState.Ready(cached, loader, identity, stale = Stale.Waking))
                    }
                }
            },
        )
    }
    when (res) {
        is LibraryResult.Ok -> {
            if (res.games.isEmpty()) {
                set(LibState.Message(say(R.string.lib_empty)))
            } else {
                set(LibState.Ready(res.games, loader, identity))
                // Remembered AFTER it is on screen: the disk write is not on the path to a shelf.
                withContext(Dispatchers.IO) { cache.store(key, res.profile, res.games) }
                val status = withContext(Dispatchers.IO) {
                    LibraryClient.fetchStatus(
                        address = host.address,
                        mgmtPort = host.effectiveMgmtPort,
                        certPem = identity.certPem,
                        keyPem = identity.privateKeyPem,
                        fpHex = host.fpHex,
                        streamPort = host.port,
                    )
                }
                set(LibState.Ready(res.games, loader, identity).withStatus(status))
            }
            val listed = res.profile?.let {
                withContext(Dispatchers.IO) {
                    HostProfiles.fetch(identity, host.address, host.effectiveMgmtPort, host.fpHex, host.port)
                } as? ProfilesAnswer.Listed
            }
            offer(Hide.offer(listed?.rows, res.profile))
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
                        is LibraryResult.Unauthorized -> say(R.string.lib_load_failed_why, res.message)
                        is LibraryResult.Error -> say(R.string.lib_load_failed_why, res.message)
                        else -> say(R.string.lib_load_failed)
                    },
                )
            },
        )
    }
}

/**
 * What [profile] hides on [host], fetched each time it is asked for and never cached. The
 * shelf's identity and art loader when [shelf] has them. A failure is the shelf's own line.
 */
private suspend fun loadHidden(context: Context, host: KnownHost, profile: String, shelf: LibState): LibState {
    val (identity, loader) = (shelf as? LibState.Ready)?.let { it.identity to it.loader }
        ?: prepareLoader(context, host)
        ?: return LibState.Message(say(R.string.android_lib_identity_unavailable))
    val res = withContext(Dispatchers.IO) {
        Hide.fetch(identity, host.address, host.effectiveMgmtPort, host.fpHex, profile, host.port)
    }
    return when (res) {
        is LibraryResult.Ok -> LibState.Ready(res.games, loader, identity)
        is LibraryResult.Unauthorized -> LibState.Message(say(R.string.lib_load_failed_why, res.message))
        is LibraryResult.Error -> LibState.Message(say(R.string.lib_load_failed_why, res.message))
    }
}

/**
 * The identity and the art loader, needed whether or not the host ever answers — cached posters
 * have to render behind a cached catalog with the box still down. Null = no identity.
 */
internal suspend fun prepareLoader(context: Context, host: KnownHost): Pair<ClientIdentity, ImageLoader>? =
    withContext(Dispatchers.IO) {
        val id = IdentityHolder.shared(context).await() ?: return@withContext null
        val loader = runCatching { posterLoader(context, id, host.address, host.fpHex, host.port) }.getOrNull()
            ?: return@withContext null
        id to loader
    }

