package io.unom.punktfunk

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.ContentTransform
import androidx.compose.animation.AnimatedContentTransitionScope
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Icon
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.NavigationRail
import androidx.compose.material3.NavigationRailItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import android.Manifest
import android.os.Build
import android.widget.Toast
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.compose.ui.res.stringResource
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.discovery.HostDiscovery
import io.unom.punktfunk.kit.link.DeepLinkResult
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.link.HostResolution
import io.unom.punktfunk.kit.link.StartScreen
import io.unom.punktfunk.kit.link.host
import io.unom.punktfunk.kit.SessionEndReason
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.console.SkiaConsole
import io.unom.punktfunk.console.SkiaConsoleShell
import io.unom.punktfunk.models.LibraryReturn
import io.unom.punktfunk.models.Tab
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

@Composable
fun App(forceGamepadUi: Boolean = false) {
    val context = LocalContext.current
    val activity = context as? MainActivity
    val settingsStore = remember { SettingsStore(context) }
    var settings by remember { mutableStateOf(settingsStore.load()) }
    // The active session (null = not streaming), held for the process: a recreated activity shows
    // the same stream. It carries the settings the connect resolved.
    val session = SessionGate.owner.session
    val owner = SessionGate.owner
    var tab by remember { mutableStateOf(Tab.Connect) }
    // Set when a session ends because its game exited and it began as a library launch: the host
    // whose library the console shell should come back to. Held HERE because the shell's own
    // navigation state does not outlive the stream. Cleared once the shell has consumed it, so a
    // later manual Back out of the library is not undone by a stale value.
    var reopenLibrary by remember { mutableStateOf<LibraryReturn?>(null) }
    // The shelf the Library tab shows, as a [Shelf] id: a card's Browse library, Start in and a
    // game's exit set it; null is the shelf the tab last showed. Held here beside `tab`: a
    // `remember` inside the tab does not outlive the stream a launch off the shelf starts.
    var shelfId by remember { mutableStateOf<String?>(null) }
    fun showShelf(hostId: String, presetId: String?) {
        shelfId = Shelf.id(hostId, presetId).also { LibraryLayout.rememberShelf(context, it) }
        tab = Tab.Library
    }
    // Whether that shelf dials the host's desktop as it opens (`start_in = stream`). Only the
    // cold start sets it. A launch off the shelf, a picked shelf and a tab switch disarm it: a
    // shelf composed again would read it as a cold start and dial again.
    var touchAutoStream by remember { mutableStateOf(false) }

    // A stream's capture hides the pad it claims (a Sony pad's forced USB claim, the paused SC2
    // menu capture) until the stream screen is disposed, after its fade-out. Held from a console
    // launch until [PAD_RETURN_MS] past the session, so that gap keeps the console up.
    var padHeldByStream by remember { mutableStateOf(false) }
    LaunchedEffect(session, padHeldByStream) {
        if (session != null || !padHeldByStream) return@LaunchedEffect
        delay(PAD_RETURN_MS)
        padHeldByStream = false
    }

    // Console (gamepad) mode mirrors the Apple client: the setting AND (its mode says Always OR a
    // pad is attached OR this is a TV OR the dev force flag). Flips live as controllers
    // connect/disconnect — unless the mode is Always, where it simply stays.
    val tv = remember { isTvDevice(context) }
    val controllerConnected by rememberControllerConnected()
    // …AND the native console host is in this build (every shipping ABI today; the sysprop
    // `debug.punktfunk.console_backend=none` forces the touch UI for on-glass triage). Without a
    // console to draw, a controller drives the touch UI through Compose's own focus.
    val skiaConsole = remember { SkiaConsole.wanted() }
    // …AND it actually came up: a console whose native create failed or whose render thread died
    // ([SkiaConsole.healthy], observable) would front a SurfaceView nothing ever paints — a gray
    // screen with a working pad probe, which is worse than the touch UI it replaced.
    val gamepadUi = skiaConsole && SkiaConsole.healthy && gamepadUiActive(
        settings.gamepadUiEnabled, settings.gamepadUiMode, controllerConnected || padHeldByStream, tv,
        forceGamepadUi,
    )

    // The runtime grant both shells need before the network works, asked above them: the console
    // shell fronts a phone the moment a pad is attached, and an ask that lived only in the touch
    // screen never ran there. Android 17 blocks the browse, the probes and the dial without
    // ACCESS_LOCAL_NETWORK; NEARBY_WIFI_DEVICES (API 33+) is the multicast hedge some OEMs want.
    var lnpGranted by remember { mutableStateOf(hasLocalNetworkPermission(context)) }
    val discovery = remember { HostDiscovery.shared(context) }
    val localNetLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        lnpGranted = granted
        // A browse started before the grant has dead sockets. The touch grid shows a denial as
        // its banner; the console has only a notice.
        if (granted) {
            discovery.restart()
        } else if (gamepadUi) {
            SkiaConsole.notice(context.getString(R.string.android_lnp_denied_body))
        }
    }
    val nearbyLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { _ -> /* a hint only; the browse runs either way */ }
    LaunchedEffect(Unit) {
        if (!lnpGranted) {
            localNetLauncher.launch(Manifest.permission.ACCESS_LOCAL_NETWORK)
        } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && !hasNearbyPermission(context)) {
            // On 37+ the two share a group, so the grant above already covers this one.
            nearbyLauncher.launch(Manifest.permission.NEARBY_WIFI_DEVICES)
        }
    }
    // A grant made in system settings neither kills nor notifies the app; the return does.
    DisposableEffect(Unit) {
        val lifecycle = activity?.lifecycle ?: return@DisposableEffect onDispose {}
        val obs = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME && !lnpGranted && hasLocalNetworkPermission(context)) {
                lnpGranted = true
                discovery.restart()
            }
        }
        lifecycle.addObserver(obs)
        onDispose { lifecycle.removeObserver(obs) }
    }

    // The key service's disclosure, once, in normal use: Play rejects one reachable only from
    // Settings. Only where the service matters (a hardware keyboard, not a TV), never mid-stream.
    var keyDisclosure by remember {
        mutableStateOf(!tv && hasPhysicalKeyboard() && !KeyCaptureService.disclosureAnswered(context))
    }
    if (keyDisclosure && session == null && !KeyCaptureService.running) {
        KeyCaptureDisclosure(onDismiss = {
            keyDisclosure = false
            KeyCaptureService.markDisclosureAnswered(context)
        })
    }

    // System bars have ONE owner: this effect. The stream and the console shell both want the
    // whole panel (bars hidden, a swipe shows them transiently); the touch shell wants them back.
    // It cannot live inside the screens themselves: `AnimatedContent` below keeps the outgoing
    // screen composed until its fade ends, so a per-screen `onDispose { show(...) }` fired AFTER
    // the incoming screen's hide — console → stream left the status and gesture bars parked over
    // the video. Keyed on the resolved intent, not the screens.
    val immersive = session != null || gamepadUi
    DisposableEffect(immersive) {
        val window = activity?.window ?: return@DisposableEffect onDispose {}
        val controller = WindowCompat.getInsetsController(window, window.decorView)
        if (immersive) {
            controller.systemBarsBehavior =
                WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            controller.hide(WindowInsetsCompat.Type.systemBars())
        } else {
            controller.show(WindowInsetsCompat.Type.systemBars())
        }
        onDispose {}
    }
    // Orientation has the same one owner, for the same reason. A hinge across the stream lets
    // go, so a clamshell half-folded on a table stands upright and takes the tabletop split.
    val hinge = rememberFoldHinge()
    HoldLandscape("shell", if (session != null) hinge == null else gamepadUi)

    // A link may not preempt a live session. MainActivity.onCreate refuses one that arrives as a
    // second instance; this is the rare in-instance case (a caller that set FLAG_ACTIVITY_SINGLE_TOP,
    // so the link reached `onNewIntent` on the streaming activity itself). Pointing at the host
    // already being streamed is the one exception, and its right answer is to do nothing — the
    // intent has already brought the app forward, which is exactly what "focus it" means here.
    val pendingLink = activity?.pendingDeepLink
    LaunchedEffect(pendingLink, session) {
        val url = pendingLink ?: return@LaunchedEffect
        // Not streaming: the Hosts tab routes it, so a link that finds another tab up shows it.
        val live = session ?: run {
            tab = Tab.Connect
            return@LaunchedEffect
        }
        activity.pendingDeepLink = null
        val parsed = DeepLinks.parse(url) as? DeepLinkResult.Parsed ?: return@LaunchedEffect
        val target = DeepLinks.resolveHost(parsed.link, KnownHostStore(context).all())
        val sameHost = target is HostResolution.Record && target.host.id == live.hostId
        if (!sameHost) {
            Toast.makeText(
                context,
                context.getString(R.string.android_already_streaming),
                Toast.LENGTH_LONG,
            ).show()
        }
    }

    // Where a bare launch opens (design/default-host.md). Once per PROCESS, not per composition:
    // `LaunchedEffect(Unit)` in this composable runs on a cold start and never again, so
    // foregrounding, a finished stream and a return from Settings all land where the shell would
    // land anyway. Never with a link waiting — explicit intent wins — and never while the console
    // shell is up, which resolves the same policy for itself in `SkiaConsole.ensure`.
    LaunchedEffect(Unit) {
        if (gamepadUi || pendingLink != null || session != null) return@LaunchedEffect
        val hosts = KnownHostStore(context).all()
        val start = StartScreen.resolve(settings.startIn, settings.defaultHost, hosts)
        val host = start.host ?: return@LaunchedEffect
        // Stream is the library plus a connect, not a fourth screen: the shelf goes up either
        // way, so a cancelled or refused dial lands there rather than back on the host grid.
        showShelf(host.id, null)
        touchAutoStream = start is StartScreen.Stream
    }

    // The touch shell's half of "come back to the library this game was launched from" — the console
    // shell consumes the same intent on its way in (see SkiaConsoleShell). Gated on which shell is up so
    // exactly one of the two ever claims it, and cleared here so a later manual Back stays backed
    // out. A host forgotten while the game ran simply leaves us on the host grid.
    LaunchedEffect(reopenLibrary, gamepadUi) {
        if (gamepadUi) return@LaunchedEffect
        val (id, pinId) = reopenLibrary ?: return@LaunchedEffect
        KnownHostStore(context).all().firstOrNull { it.id == id }?.let { kh ->
            // A pin unpinned while the game was running is no longer a shelf: fall back to the
            // host's own, rather than a card that no longer exists.
            showShelf(kh.id, pinId?.takeIf { it in kh.pinnedPresetIds })
        }
        reopenLibrary = null
    }

    // A lost session dials the same host again on the schedule every client shares; the host
    // resumes the session it still holds. The connect modal stays up between dials and Cancel
    // stops them. A refusal or a spent schedule ends it with a toast. A redial carries no unlock:
    // a locked profile the host no longer admits goes to the key unlock or the password prompt.
    val redialScope = rememberCoroutineScope()
    var redialing by remember { mutableStateOf<Pair<String, Job>?>(null) }
    val redialServices = remember { HostServices(KnownHostStore(context), JniHostNative(context)) }
    var redialUnlock by remember { mutableStateOf<PasswordAsk?>(null) }
    suspend fun redialOnce(spec: DialSpec, name: String): HostServices.Dialed {
        val record = redialServices.store.resolve(spec.pinHex, spec.host, spec.port)
        val hooks = record?.takeIf { spec.promptsUnlock }?.let { r ->
            touchUnlockHooks(context, spec.identity, r, ask = { ask ->
                redialUnlock = ask
                try {
                    ask.answer.await()
                } finally {
                    redialUnlock = null
                }
            })
        }
        val row = spec.profile?.let { savedRow(it, record?.asProfile) }
        return dialUnlocked(hooks, spec.pinHex, row) { unlock -> redialServices.dial(spec, record, name, unlock = unlock) }
    }
    fun redial(spec: DialSpec, name: String) {
        val job = redialScope.launch {
            var attempt = 0
            while (true) {
                val wait = SessionEndReason.LOST.redialDelayMs(attempt++)
                if (wait == null) {
                    Toast.makeText(
                        context,
                        context.getString(R.string.android_redial_gave_up, name),
                        Toast.LENGTH_LONG,
                    ).show()
                    break
                }
                delay(wait)
                when (val dialed = redialOnce(spec, name)) {
                    is HostServices.Connected -> {
                        owner.open(dialed.session)
                        break
                    }
                    is HostServices.Failed -> if (!dialed.unreachable) {
                        Toast.makeText(
                            context,
                            dialed.said.ifEmpty {
                                context.getString(ConnectErrors.connectMessage(dialed.token, requestAccess = false))
                            },
                            Toast.LENGTH_LONG,
                        ).show()
                        break
                    }
                    HostServices.Cancelled -> break
                }
            }
            redialing = null
        }
        redialing = name to job
    }
    // A woken host that answered before it could stream: one dial after [waitMs], which keeps
    // dialing while the host refuses inside its window. Past it, the host's refusal shows.
    fun redialWaking(spec: DialSpec, name: String, waitMs: Long) {
        val job = redialScope.launch {
            delay(waitMs)
            when (val dialed = redialOnce(spec, name)) {
                is HostServices.Connected -> owner.open(dialed.session)
                is HostServices.Failed -> Toast.makeText(
                    context,
                    dialed.said.ifEmpty {
                        context.getString(ConnectErrors.connectMessage(dialed.token, requestAccess = false))
                    },
                    Toast.LENGTH_LONG,
                ).show()
                HostServices.Cancelled -> {}
            }
            redialing = null
        }
        redialing = name to job
    }
    redialing?.takeIf { redialUnlock == null }?.let { (name, job) ->
        ConnectModal(ConnectPhase.Connecting(name), onCancel = { job.cancel(); redialing = null }, onRetry = {})
    }
    redialUnlock?.let { UnlockPromptDialog(it) }

    AnimatedContent(
        targetState = session,
        transitionSpec = {
            fadeIn() togetherWith fadeOut()
        },
        label = "StreamTransition"
    ) { active ->
        when {
            active != null -> StreamScreen(
                active,
                hinge,
                wakeRedial = {
                    val waking = redialServices.wakeRedial(active)
                    if (waking != null) {
                        if (skiaConsole) SkiaConsole.sessionEnded(null)
                        owner.end(active.handle)
                        val name = active.hostId?.let { redialServices.store.byId(it)?.name } ?: waking.first.host
                        redialWaking(waking.first, name, waking.second)
                    }
                    waking != null
                },
            ) { reason ->
                // A game launched from a library exiting is a normal finish, and the player is
                // almost certainly after the next title — so send them back to that library rather
                // than all the way out to host selection. The console shell's own screen state does
                // not survive the stream (StreamScreen replaces it in the composition, discarding
                // its `remember`s), so the intent is hoisted here and handed back on the way in.
                reopenLibrary =
                    if (reason == SessionEndReason.GAME_EXITED && active.launchedFromLibrary) {
                        active.hostId?.let { LibraryReturn(it, active.libraryPresetId) }
                    } else {
                        null
                    }
                val redialSpec = active.redial.takeIf { reason == SessionEndReason.LOST }
                // The console keeps its stack across the stream and wants to know how the session
                // ended — a clean end is no toast, an abnormal one says why (the desktop shell's
                // exact contract). A redial says it itself.
                if (skiaConsole) {
                    SkiaConsole.sessionEnded(abnormalEndMessage(reason).takeIf { redialSpec == null })
                }
                owner.end(active.handle)
                if (redialSpec != null) {
                    val name = active.hostId?.let { redialServices.store.byId(it)?.name } ?: redialSpec.host
                    redial(redialSpec, name)
                }
            }
            // The console: the same Skia shell the Linux/Windows session binary shows, drawn by
            // native onto a SurfaceView (design/android-skia-console-port.md). `gamepadUi` already
            // folds in whether the native host is present on this build.
            gamepadUi -> SkiaConsoleShell(
                settings = settings,
                onSettingsChange = { settings = it; settingsStore.save(it) },
                onConnected = { padHeldByStream = true; owner.open(it) },
                deepLink = pendingLink,
                onDeepLinkHandled = { activity?.pendingDeepLink = null },
                reopenLibrary = reopenLibrary,
                onReopenLibraryHandled = { reopenLibrary = null },
            )
            else -> TouchTabs(tab, onTab = { tab = it; touchAutoStream = false }) { targetTab ->
                when (targetTab) {
                    Tab.Connect -> ConnectScreen(
                        settings = settings,
                        onConnected = { owner.open(it) },
                        onSettingsChange = { settings = it; settingsStore.save(it) },
                        deepLink = pendingLink,
                        onDeepLinkHandled = { activity?.pendingDeepLink = null },
                        // A card's Browse library: the Library tab, on that card's shelf.
                        onOpenLibrary = { kh, pinId -> showShelf(kh.id, pinId) },
                        lnpGranted = lnpGranted,
                        onAskLocalNetwork = {
                            localNetLauncher.launch(Manifest.permission.ACCESS_LOCAL_NETWORK)
                        },
                    )
                    Tab.Library -> LibraryTab(
                        settings = settings,
                        shelfId = shelfId,
                        onShelf = { id ->
                            shelfId = id
                            LibraryLayout.rememberShelf(context, id)
                            touchAutoStream = false
                        },
                        autoStream = touchAutoStream,
                        lnpGranted = lnpGranted,
                        onLaunched = { touchAutoStream = false; owner.open(it) },
                        onShowHosts = { tab = Tab.Connect },
                    )
                    Tab.Settings -> SettingsScreen(
                        initial = settings,
                        onChange = { settings = it; settingsStore.save(it) },
                        onBack = { tab = Tab.Connect },
                    )
                }
            }
        }
    }
}

/**
 * How long the console outlives a hidden pad after a stream: the fade-out, a USB re-enumeration
 * and a BLE SC2 re-capture. A pad unplugged mid-stream keeps the console this much longer.
 */
private const val PAD_RETURN_MS = 3_000L

/** What the console toasts when a session ends; null for a clean end. */
private fun abnormalEndMessage(reason: SessionEndReason): String? = when (reason) {
    SessionEndReason.NONE, SessionEndReason.LOCAL,
    SessionEndReason.GAME_EXITED, SessionEndReason.HOST_ENDED -> null
    SessionEndReason.HOST_ERROR -> "the host reported an error"
    SessionEndReason.LOST -> "the connection was lost"
}

/**
 * The touch shell's adaptive nav around [content]: a bottom bar on phones; on tablets / large
 * windows a side NavigationRail with its items centred vertically (the common Android tablet
 * idiom, mirroring iPad's side navigation). A short landscape phone keeps the bottom bar (the
 * rail needs height too).
 */
@Composable
private fun TouchTabs(tab: Tab, onTab: (Tab) -> Unit, content: @Composable (Tab) -> Unit) {
    // Tabs slide along the axis the nav sits on: horizontally with the bottom bar (phone),
    // vertically with the side rail (tablet), so the motion tracks the direction you moved.
    val tabContent: @Composable (vertical: Boolean) -> Unit = { vertical ->
        AnimatedContent(
            targetState = tab,
            transitionSpec = { tabTransition(vertical) },
            label = "TabTransition",
        ) { content(it) }
    }
    BoxWithConstraints(Modifier.fillMaxSize()) {
        if (maxWidth >= 600.dp && maxHeight >= 480.dp) {
            Row(Modifier.fillMaxSize()) {
                NavigationRail(Modifier.fillMaxHeight()) {
                    Spacer(Modifier.weight(1f)) // centre the rail items vertically
                    Tab.entries.forEach { t ->
                        NavigationRailItem(
                            selected = tab == t,
                            onClick = { onTab(t) },
                            icon = { Icon(t.icon, contentDescription = stringResource(t.label)) },
                            label = { Text(stringResource(t.label)) },
                        )
                    }
                    Spacer(Modifier.weight(1f))
                }
                // The rail handles its own insets; the content pane insets itself (the screens
                // don't, since they used to rely on the Scaffold's padding). Cutout included:
                // a tablet in landscape puts its punch on exactly this pane's leading edge.
                Box(Modifier.weight(1f).fillMaxHeight().safeArea()) { tabContent(true) }
            }
        } else {
            Scaffold(
                bottomBar = {
                    NavigationBar {
                        Tab.entries.forEach { t ->
                            NavigationBarItem(
                                selected = tab == t,
                                onClick = { onTab(t) },
                                icon = { Icon(t.icon, contentDescription = stringResource(t.label)) },
                                label = { Text(stringResource(t.label)) },
                            )
                        }
                    }
                },
            ) { innerPadding ->
                Box(Modifier.fillMaxSize().padding(innerPadding)) { tabContent(false) }
            }
        }
    }
}

/** Slide forward along the nav's axis, back the other way; fade either way. */
private fun AnimatedContentTransitionScope<Tab>.tabTransition(vertical: Boolean): ContentTransform {
    val forward = targetState.ordinal > initialState.ordinal
    return when {
        vertical && forward ->
            slideInVertically { it } + fadeIn() togetherWith slideOutVertically { -it } + fadeOut()
        vertical ->
            slideInVertically { -it } + fadeIn() togetherWith slideOutVertically { it } + fadeOut()
        forward ->
            slideInHorizontally { it } + fadeIn() togetherWith slideOutHorizontally { -it } + fadeOut()
        else ->
            slideInHorizontally { -it } + fadeIn() togetherWith slideOutHorizontally { it } + fadeOut()
    }
}
