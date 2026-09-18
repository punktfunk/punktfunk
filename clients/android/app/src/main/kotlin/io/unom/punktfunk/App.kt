package io.unom.punktfunk

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.ContentTransform
import androidx.compose.animation.AnimatedContentTransitionScope
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.ScrollState
import androidx.compose.foundation.gestures.animateScrollBy
import androidx.compose.foundation.gestures.scrollBy
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Icon
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.NavigationRail
import androidx.compose.material3.NavigationRailItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Density
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
import io.unom.punktfunk.kit.discovery.HostDiscovery
import io.unom.punktfunk.kit.link.DeepLinkResult
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.link.HostResolution
import io.unom.punktfunk.kit.link.StartScreen
import io.unom.punktfunk.kit.link.host
import io.unom.punktfunk.kit.SessionEndReason
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.models.ActiveSession
import io.unom.punktfunk.console.SkiaConsole
import io.unom.punktfunk.console.SkiaConsoleShell
import io.unom.punktfunk.models.LibraryReturn
import io.unom.punktfunk.models.Tab
import kotlin.math.roundToInt
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

@Composable
fun App(forceGamepadUi: Boolean = false) {
    val context = LocalContext.current
    val activity = context as? MainActivity
    val settingsStore = remember { SettingsStore(context) }
    var settings by remember { mutableStateOf(settingsStore.load()) }
    // The active session (null = not streaming). It carries the settings the connect resolved,
    // so the stream screen never re-reads the store behind its own connect's back.
    var session by remember { mutableStateOf<ActiveSession?>(null) }
    var tab by remember { mutableStateOf(Tab.Connect) }
    // Set when a session ends because its game exited and it began as a library launch: the host
    // whose library the console shell should come back to. Held HERE because the shell's own
    // navigation state does not outlive the stream. Cleared once the shell has consumed it, so a
    // later manual Back out of the library is not undone by a stale value.
    var reopenLibrary by remember { mutableStateOf<LibraryReturn?>(null) }
    // Which host's game library the TOUCH shell is showing, and which of its pinned cards opened it
    // (null = the host's own card). Held here beside `tab` rather than inside the tab content: it is
    // a PUSH over the whole shell, and a `remember` down in ConnectScreen would not survive the
    // stream that a launch off the shelf starts — which is exactly what `reopenLibrary` restores.
    var touchLibrary by remember { mutableStateOf<Pair<KnownHost, String?>?>(null) }
    // …and whether that shelf should dial the host's desktop as it opens (`start_in = stream`).
    // Only the cold-start effect below ever sets it; every other route to the library is a
    // deliberate press and must not start a stream on its own. Any launch off the shelf disarms
    // it: the stream screen replaces the shelf, so ending one composes a fresh shelf that would
    // otherwise read this as a cold start and dial again.
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
            SkiaConsole.notice(
                "Punktfunk can't find or reach hosts without local network access. " +
                    "Allow it in Android's app settings.",
            )
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

    // Publish the live session process-wide: a `punktfunk://` link that arrives as a SECOND
    // activity instance (the normal case under `launchMode = standard`) refuses it before that
    // instance is resumed (MainActivity.onCreate), and every shell's dial refuses to open a second
    // session behind it. Cleared on dispose, so an activity destroyed mid-stream leaves no ghost.
    DisposableEffect(session) {
        SessionGate.live = session?.let { SessionGate.Live(it.hostId) }
        onDispose { SessionGate.live = null }
    }

    // The same rule for the rare in-instance case (a caller that set FLAG_ACTIVITY_SINGLE_TOP, so
    // the link reached `onNewIntent` on the streaming activity itself). Pointing at the host
    // already being streamed is the one exception, and its right answer is to do nothing — the
    // intent has already brought the app forward, which is exactly what "focus it" means here.
    val pendingLink = activity?.pendingDeepLink
    LaunchedEffect(pendingLink, session) {
        val url = pendingLink ?: return@LaunchedEffect
        val live = session ?: return@LaunchedEffect // not streaming: ConnectScreen routes it
        activity.pendingDeepLink = null
        val parsed = DeepLinks.parse(url) as? DeepLinkResult.Parsed ?: return@LaunchedEffect
        val target = DeepLinks.resolveHost(parsed.link, KnownHostStore(context).all())
        val sameHost = target is HostResolution.Record && target.host.id == live.hostId
        if (!sameHost) {
            Toast.makeText(
                context,
                "Already streaming — end this session first.",
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
        // Stream is the library PLUS a connect, not a fourth screen: the shelf goes up either
        // way, so a cancelled or refused dial lands there rather than back on the host grid.
        touchLibrary = host to null
        touchAutoStream = start is StartScreen.Stream
    }

    // The touch shell's half of "come back to the library this game was launched from" — the console
    // shell consumes the same intent on its way in (see GamepadShell). Gated on which shell is up so
    // exactly one of the two ever claims it, and cleared here so a later manual Back stays backed
    // out. A host forgotten while the game ran simply leaves us on the host grid.
    LaunchedEffect(reopenLibrary, gamepadUi) {
        if (gamepadUi) return@LaunchedEffect
        val (id, pinId) = reopenLibrary ?: return@LaunchedEffect
        KnownHostStore(context).all().firstOrNull { it.id == id }?.let { kh ->
            // A pin unpinned while the game was running is no longer a shelf: fall back to the
            // host's own, rather than a card that no longer exists.
            touchLibrary = kh to pinId?.takeIf { it in kh.pinnedPresetIds }
        }
        reopenLibrary = null
    }

    // The console backdrop's colour family, published once from the live settings rather than
    // threaded through every screen that draws a backdrop. Because it is read from the SAME
    // `settings` state the gamepad settings screen writes, stepping the Background row recolours
    // the field behind that very row.
    val palette = GamepadPalette.named(settings.uiPalette)
    CompositionLocalProvider(
        LocalGamepadPalette provides palette,
        LocalGamepadInk provides GamepadInk.of(palette),
    ) {
    AnimatedContent(
        targetState = session,
        transitionSpec = {
            fadeIn() togetherWith fadeOut()
        },
        label = "StreamTransition"
    ) { active ->
        // Read once: `touchLibrary` is a `var`, so it does not smart-cast through the branch.
        val library = touchLibrary
        when {
            active != null -> StreamScreen(active) { reason ->
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
                // The console keeps its stack across the stream and wants to know how the session
                // ended — a clean end is no toast, an abnormal one says why (the desktop shell's
                // exact contract).
                if (skiaConsole) SkiaConsole.sessionEnded(abnormalEndMessage(reason))
                session = null
            }
            // The console: the same Skia shell the Linux/Windows session binary shows, drawn by
            // native onto a SurfaceView (design/android-skia-console-port.md). `gamepadUi` already
            // folds in whether the native host is present on this build.
            gamepadUi -> SkiaConsoleShell(
                settings = settings,
                onSettingsChange = { settings = it; settingsStore.save(it) },
                onConnected = { padHeldByStream = true; session = it },
                deepLink = pendingLink,
                onDeepLinkHandled = { activity?.pendingDeepLink = null },
                reopenLibrary = reopenLibrary,
                onReopenLibraryHandled = { reopenLibrary = null },
            )
            // The touch shell's library is a PUSHED screen, not a tab: it belongs to one host, so
            // it takes the whole window (bar included) and Back — the arrow or the system gesture —
            // returns to the grid.
            library != null -> LibraryScreen(
                host = library.first,
                settings = settings,
                onLaunched = { touchAutoStream = false; session = it },
                onBack = {
                    touchLibrary = null
                    touchAutoStream = false
                },
                pinnedPresetId = library.second,
                autoStream = touchAutoStream,
            )
            else -> TouchTabs(tab, onTab = { tab = it }) { targetTab ->
                when (targetTab) {
                    Tab.Connect -> ConnectScreen(
                        settings = settings,
                        onConnected = { session = it },
                        onSettingsChange = { settings = it; settingsStore.save(it) },
                        deepLink = pendingLink,
                        onDeepLinkHandled = { activity?.pendingDeepLink = null },
                        // "Browse library…" in a card's overflow — the touch route to the shelf
                        // the console shell reaches with Y.
                        onOpenLibrary = { kh, pinId -> touchLibrary = kh to pinId },
                        lnpGranted = lnpGranted,
                        onAskLocalNetwork = {
                            localNetLauncher.launch(Manifest.permission.ACCESS_LOCAL_NETWORK)
                        },
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
                            icon = { Icon(t.icon, contentDescription = t.label) },
                            label = { Text(t.label) },
                        )
                    }
                    Spacer(Modifier.weight(1f))
                }
                // The rail handles its own insets; the content pane insets itself (the screens
                // don't, since they used to rely on the Scaffold's padding). Cutout included:
                // a tablet in landscape puts its punch on exactly this pane's leading edge.
                Box(Modifier.weight(1f).fillMaxHeight().consoleSafeArea()) { tabContent(true) }
            }
        } else {
            Scaffold(
                bottomBar = {
                    NavigationBar {
                        Tab.entries.forEach { t ->
                            NavigationBarItem(
                                selected = tab == t,
                                onClick = { onTab(t) },
                                icon = { Icon(t.icon, contentDescription = t.label) },
                                label = { Text(t.label) },
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

/**
 * The console backdrop's colour family for everything under [App] — provided from the live
 * settings so a change on the gamepad settings screen recolours every backdrop at once. Defaults
 * to the brand violet, which is also what a preview or a test composition gets.
 */
val LocalGamepadPalette = compositionLocalOf { GamepadPalette.named("violet") }

/**
 * Re-inks a screen written against the TOUCH theme so it can be shown on the console's field.
 *
 * `ControllersScreen` alone pulls `MaterialTheme.colorScheme` at 27 explicit sites, plus implicitly
 * through every `OutlinedCard`, `Switch`, `OutlinedButton` and `LinearProgressIndicator` it draws.
 * Dropped into the shell those keep the touch palette — light-grey body text with no background of
 * its own, which over the six PALE console palettes (`GamepadPalette`, `light = true`) is grey on
 * pastel: technically painted, in practice unreadable. That is the same class of bug as the console
 * dialogs that spent a release rendering dark ink on a dark card.
 *
 * The fix is deliberately ONE derived colour scheme rather than 27 call-site branches:
 *  * a call-site branch cannot reach the IMPLICIT pulls at all — a `Switch`'s track and an
 *    `OutlinedCard`'s border are resolved inside Material, not here;
 *  * two colours per site is exactly the shape that drifts, and it would leave the touch screen
 *    carrying console vocabulary it has no use for.
 *
 * The alternative — give the console presentation an opaque backdrop and let the touch theme read on
 * its own ground — was rejected because it splits the screen's material in two: an opaque touch-grey
 * slab under a palette-inked header and legend, with a visible seam between them, on a field whose
 * whole point is that one look runs through it.
 *
 * The base scheme follows the field's lightness, so anything not overridden here (a container role
 * some Material component reaches for) still lands on the right side of the contrast line.
 */
@Composable
internal fun ConsoleInkedTheme(content: @Composable () -> Unit) {
    val ink = LocalGamepadInk.current
    val scheme = remember(ink) {
        val base = if (ink.isLight) lightColorScheme() else darkColorScheme()
        base.copy(
            primary = ink.accent,
            onPrimary = ink.onAccent,
            // A card becomes a PANE over the aurora rather than a slab on top of it: the console's
            // own glass fill, so an OutlinedCard here is cut from the material the settings rows are.
            surface = ink.glass,
            onSurface = ink.fg,
            surfaceVariant = ink.fg(0.12f),
            onSurfaceVariant = ink.fg(0.68f),
            outline = ink.fg(0.30f),
            outlineVariant = ink.fg(0.16f),
            // Nothing here paints a background — the aurora is the ground — but a component that
            // resolves `background` (or the content colour for it) must still land on the palette.
            background = Color.Transparent,
            onBackground = ink.fg,
        )
    }
    // The typography and shapes are the app's, not Material's defaults: this swaps the INK, not the
    // brand typeface. And `LocalContentColor` has to be provided by hand — outside a Surface or a
    // Scaffold it defaults to BLACK, which is how an unstyled `Text` would vanish into a dark field.
    MaterialTheme(
        colorScheme = scheme,
        typography = MaterialTheme.typography,
        shapes = MaterialTheme.shapes,
    ) {
        CompositionLocalProvider(LocalContentColor provides ink.fg, content = content)
    }
}

/**
 * The console's scroll route for a screen that is a WALL of content rather than a list of focusable
 * rows.
 *
 * Compose only scrolls a container to keep a FOCUSED child visible, so a screen whose body holds no
 * focusable nodes (the licenses notices are one enormous `Text`) simply cannot be scrolled by a
 * controller: the D-pad has nothing to move to. These screens therefore drive the scroll state
 * directly — up/down steps, the shoulders page.
 *
 * Returned as a plain function so a screen's nav callbacks read `scroll(-1, page = false)` rather
 * than each screen minting its own coroutine + viewport arithmetic (which is how the two would end
 * up scrolling at different speeds).
 */
@Composable
internal fun rememberConsoleScroller(scroll: ScrollState): (dir: Int, page: Boolean) -> Unit {
    val scope = rememberCoroutineScope()
    val animated = animationsEnabled()
    return remember(scroll, animated) {
        { dir, page ->
            val delta = consoleScrollDelta(scroll.viewportSize.toFloat(), page, dir)
            if (delta != 0f) {
                scope.launch {
                    // Auto-repeat fires every 150 ms while a direction is held, so each animation is
                    // short enough to have landed (or nearly) before the next one cancels it —
                    // otherwise a held D-pad crawls, each step restarting from where the last was
                    // interrupted.
                    if (animated) {
                        scroll.animateScrollBy(
                            delta,
                            ConsoleMotion.ease(
                                if (page) ConsoleMotion.TRANSITION_MS else ConsoleMotion.FOCUS_MS,
                            ),
                        )
                    } else {
                        scroll.scrollBy(delta)
                    }
                }
            }
        }
    }
}

/**
 * How far one console scroll press travels: [dir] is -1 (up/left) or +1 (down/right), [page] picks
 * the shoulders' full page over a D-pad step. Zero while the viewport is unmeasured — a first press
 * that arrived before layout must do nothing rather than fling the content by zero-times-nothing.
 */
internal fun consoleScrollDelta(viewportPx: Float, page: Boolean, dir: Int): Float =
    if (viewportPx <= 0f) 0f else viewportPx * (if (page) CONSOLE_PAGE else CONSOLE_STEP) * dir

/**
 * A page keeps a band of what you were reading on screen rather than jumping a clean screenful — the
 * overlap every reader has used since the printed page, and the difference between "I moved down"
 * and "where was I".
 */
private const val CONSOLE_PAGE = 0.88f

/** A D-pad step is about a quarter screen, so holding the direction walks the wall rather than flicking it. */
private const val CONSOLE_STEP = 0.28f
