package io.unom.punktfunk

import android.Manifest
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager
import android.media.audiofx.AcousticEchoCanceler
import android.media.audiofx.AudioEffect
import android.media.audiofx.NoiseSuppressor
import android.os.Build
import android.text.InputType
import android.util.Log
import android.view.KeyEvent
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.WindowInsetsSides
import androidx.compose.foundation.layout.displayCutout
import androidx.compose.foundation.layout.only
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.movableContentOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.layout.layout
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import io.unom.punktfunk.kit.GamepadRouter
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.security.IdentityLoad
import io.unom.punktfunk.kit.security.IdentityStore
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.kit.SessionAccess
import io.unom.punktfunk.kit.SessionEndReason
import io.unom.punktfunk.kit.VideoDecoders
import io.unom.punktfunk.kit.VideoFit
import io.unom.punktfunk.models.ActiveSession
import io.unom.punktfunk.kit.library.GameEnd
import io.unom.punktfunk.kit.library.LibraryClient
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import kotlin.math.roundToInt
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * The immersive stream. Everything it reads about the session comes from [session] — the settings
 * the connect actually resolved (globals, or a preset's overrides on top of them) and the HOST's
 * clipboard decision — rather than from a fresh `SettingsStore` load, which could disagree with
 * the connect that produced this handle.
 */
@Composable
fun StreamScreen(session: ActiveSession, onSessionEnded: (SessionEndReason) -> Unit) {
    val handle = session.handle
    val initialSettings = session.settings
    val micEnabled = initialSettings.micEnabled
    val context = LocalContext.current
    val activity = context as? MainActivity
    // The View hosting this composition — the one that receives the stream's touch/pointer events
    // (the gesture Box below is a Compose node inside it), so it is where unbuffered dispatch is
    // requested.
    val composeView = androidx.compose.ui.platform.LocalView.current
    val window = activity?.window
    // The negotiated stream refresh, known from the handshake (0 = unknown / older native lib) —
    // drives the panel mode pin, the render-rate vote, and the presenter's latch grid.
    val streamHz = remember(handle) { NativeBridge.nativeVideoSize(handle)?.getOrNull(2) ?: 0 }

    // The session's access level (the per-client grants of design/per-client-access.md), the
    // courtesy mirror of what the host enforces: seeded from the Welcome's advert here, kept live
    // by the 1 Hz poll below (the host's AccessUpdate messages fold latest-wins into the native
    // state). Full control + permanent — the only state an old host or an old native lib ever
    // reports — gates nothing and draws nothing: today's look, unchanged.
    val initialAccess = remember(handle) { NativeBridge.nativeAccessState(handle) }
    // The Compose state the session's peripherals write (see [StreamUi]).
    val ui = remember(handle) { StreamUi(handle, initialAccess, initialSettings.statsVerbosity, initialSettings.invertScroll) }

    // Start mic only if the user enabled it AND granted RECORD_AUDIO (else the AAudio input fails).
    val micWanted = micEnabled && ContextCompat.checkSelfPermission(
        context,
        Manifest.permission.RECORD_AUDIO,
    ) == PackageManager.PERMISSION_GRANTED

    // The Java AEC/NS pair backstopping the native VoiceCommunication capture preset, hung off the
    // audio session id `nativeStartMic` returns. Attached in surfaceCreated (where the mic starts)
    // and released on every path that stops the mic — the surface teardown AND the final dispose —
    // so a surface recreate re-attaches to the fresh stream instead of leaking effect engines.
    // All three touch points run on the main thread; a plain list is race-free.
    val micEffects = remember { mutableListOf<AudioEffect>() }

    LaunchedEffect(ui.micHint) {
        if (ui.micHint != null) {
            delay(1600)
            ui.micHint = null
        }
    }
    LaunchedEffect(ui.motionHint) {
        if (ui.motionHint) {
            // Longer than the mic chord's 1.6 s: that one confirms something the user just did,
            // this one explains something they did not, in a sentence they have to read.
            delay(6000)
            ui.motionHint = false
        }
    }
    // The start-of-stream banner: what this session's shortcuts ARE, said once.
    val banner = rememberStartBanner(handle)
    // Touch model is fixed per session (re-keys the gesture handler below if it ever changes).
    // Passthrough needs a host that injects touch; without the bit every contact would vanish, so
    // the session runs the trackpad model instead and `touchHint` below says so, once.
    val touchUnsupported = remember(handle) {
        initialSettings.touchMode == TouchMode.TOUCH && !NativeBridge.nativeHostSupportsTouch(handle)
    }
    // Live: the ring's Touch mode slot cycles it mid-stream (the gesture layer is keyed on it,
    // so a change applies from the next gesture — trap 2 in the design: never mid-gesture).
    var touchMode by remember(handle) {
        mutableStateOf(if (touchUnsupported) TouchMode.TRACKPAD else initialSettings.touchMode)
    }
    val hostAcceptsTouch = remember(handle) { NativeBridge.nativeHostSupportsTouch(handle) }
    // The quick-action ring (design/touch-client-overlay.md §2), declared ahead of the pad
    // router that opens and drives it.
    val ring = remember(handle) { RingState() }
    var containerSize by remember { mutableStateOf(IntSize.Zero) }
    // The live session mode: `nativeVideoSize` follows an accepted mode switch, but its ack lands
    // off the composition, so a request writes the asked-for mode here at once and re-reads the
    // truth shortly after (a rejection shows through then).
    var requestedMode by remember(handle) {
        mutableStateOf(NativeBridge.nativeVideoSize(handle)?.takeIf { it.size >= 2 } ?: intArrayOf(0, 0, 60))
    }
    // How the picture fills the container, and the frame it places: the SurfaceView, the touch and
    // pen lanes and the mouse all map through this one placement (kit `VideoFit`).
    val videoFit = remember(handle) { VideoFit.fromName(initialSettings.videoFit) }
    val decodedSize by rememberDecodedSize(handle)
    fun videoFrame() = decodedSize?.let { VideoFrame(videoFit, it[0], it[1]) }
        ?: VideoFrame(videoFit, requestedMode.getOrElse(0) { 0 }, requestedMode.getOrElse(1) { 0 })
    val haptics = rememberConsoleHaptics()
    val overlayCfg = remember(initialSettings.overlayActions) { OverlayConfig.parse(initialSettings.overlayActions) }
    // TV form factor (leanback): the decoder actively switches the HDMI output mode to the stream
    // refresh; a phone/tablet gets the softer seamless frame-rate hint instead.
    val isTv = remember { context.packageManager.hasSystemFeature(PackageManager.FEATURE_LEANBACK) }
    val isChromeOs = remember { context.packageManager.hasSystemFeature("org.chromium.arc") }
    // Focus anchor the soft keyboard is summoned onto AND the pointer-capture grab target (a grab
    // needs a focusable view; captured-pointer events land on it). Declared before the effect
    // below so the capture callbacks can reach the view once it exists.
    var keyCapture by remember { mutableStateOf<KeyCaptureView?>(null) }

    // The video SurfaceView, hoisted for the same reason: the pointer paths built below map WINDOW
    // coordinates onto the picture, and with a letterboxed stream that rect is the video's, not the
    // panel's. Set when the view is created.
    var videoView by remember { mutableStateOf<SurfaceView?>(null) }

    // Everything that runs beside the picture for the session — the pad router and its chords,
    // the mouse and TV-remote pointers, clipboard, feedback, sensors, the USB captures.
    val peripherals = remember(handle) {
        StreamPeripherals(
            context, activity, session, ui, ring, haptics, isTv, micEffects,
            keyCapture = { keyCapture },
            videoView = { videoView },
            containerSize = { containerSize },
            video = { videoFrame() },
            onSessionEnded = onSessionEnded,
        )
    }

    // The virtual controller (design §4), shown from the ring's `pad` slot, per session.
    var padShown by remember(handle) { mutableStateOf(false) }
    val virtualPad by rememberVirtualPad(handle, padShown, initialSettings, activity)
    var touchHint by remember { mutableStateOf(touchUnsupported) }
    LaunchedEffect(touchHint) {
        if (touchHint) {
            delay(6000)
            touchHint = false
        }
    }
    // "Low-latency mode" master toggle, resolved once for the session. On (the default) enables the
    // fast pipeline — decoder ranking + vendor keys + thread boosts (native side), HDMI ALLM below,
    // game-tagged audio, and DSCP marking (applied earlier, at connect); off runs the same decode
    // loop and presenter with plain keys and no boosts, the per-device escape hatch.
    val lowLatencyMode = initialSettings.lowLatencyMode
    // A screen with fingers on it — the start banner may only name the three-finger stats tap on a
    // device that can perform it. A TV box has no touchscreen at all, and its remote is not one.
    val keyboard = remember { !isTv && hasPhysicalKeyboard() }
    val hasTouch = remember {
        context.packageManager.hasSystemFeature(PackageManager.FEATURE_TOUCHSCREEN)
    }
    // Seed once per handle; the in-stream control must survive recomposition.
    LaunchedEffect(handle) {
        NativeBridge.nativeSetInvertScroll(handle, ui.invertScroll)
    }
    // The companion panel (design/android-dual-screen.md): a second screen below the picture —
    // the lower half of a hinge with room for it, else a small second display — carries pages the
    // player flips between. Only while streaming; showing the pad brings its page forward.
    val density = LocalDensity.current
    var rootSize by remember { mutableStateOf(IntSize.Zero) }
    val fold = rememberFoldHinge()?.let { foldSplit(it, rootSize) }
    val hingeCompanion = fold?.carriesCompanion(rootSize.height) == true
    val companionDisplay = rememberCompanionDisplay().takeUnless { hingeCompanion }
    val companionUp = hingeCompanion || companionDisplay != null
    // Spanning the picture needs the ASurfaceControl presenter's second layer: API 29 up, never
    // ChromeOS. ponytail: a static gate; an ASC init failure elsewhere leaves the second window
    // dark, which the pf-present log names.
    val spannable = remember { !isChromeOs && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q }
    // Which screen of the pair holds the picture — or both: the layout the player cycled to,
    // kept per second screen. A hinge's two halves are one display, so they share one key. A
    // two-screen console's launch starts spanned and leaves the memory alone.
    val screenKey = companionDisplay?.name ?: "hinge".takeIf { hingeCompanion }
    var layoutPick by remember(screenKey) {
        mutableStateOf(
            when {
                screenKey == null -> ScreenLayout.PANEL
                spannable && twoScreenPlatform(session.launchHold?.game?.platform) -> ScreenLayout.SPANNED
                else -> CompanionMemory.layout(context, screenKey)
            },
        )
    }
    val layoutsOffered = when {
        screenKey == null -> emptyList()
        spannable -> ScreenLayout.entries
        else -> listOf(ScreenLayout.PANEL, ScreenLayout.SWAPPED)
    }
    val layout = layoutPick.takeIf { it in layoutsOffered } ?: ScreenLayout.PANEL
    val spanned = layout == ScreenLayout.SPANNED
    // The POINTER grant gates every touch capture layer: "don't capture what can't land".
    val pointerOk = ui.accessGrants and SessionAccess.POINTER != 0
    val companionPages = companionPages(
        pointerOk,
        padShown || activity?.gamepadRouter?.sendsEnabled() == true,
        keyboard = ui.accessGrants and SessionAccess.KEYBOARD != 0,
    )
    var companionPick by remember { mutableStateOf(CompanionMemory.page(context)) }
    val companionPage = companionPick.takeIf { it in companionPages } ?: CompanionPage.STATS
    LaunchedEffect(padShown) { if (padShown) companionPick = CompanionPage.PAD }

    // The tier the HUD is polled at. `statsOn` gates the whole native pipeline: the per-frame
    // sampling (a hidden HUD costs one atomic load per frame) and the 1 s poll. With a companion
    // up the picture stays clear and the stats page is the only reader, never at Off. A 3-finger
    // tap or Select + X cycles `ui.statsVerbosity` live (Off → Compact → Normal → Detailed → Off).
    val hudTier = when {
        !companionUp -> ui.statsVerbosity
        companionPage != CompanionPage.STATS -> StatsVerbosity.OFF
        ui.statsVerbosity == StatsVerbosity.OFF -> StatsVerbosity.NORMAL
        else -> ui.statsVerbosity
    }
    val statsOn = hudTier != StatsVerbosity.OFF
    val statsLines by rememberStatsLines(session, statsOn, hudTier)

    // Host-gone watchdog and the live access level.
    SessionWatchEffect(handle, initialAccess, ui, peripherals, onSessionEnded)

    // One-shot teardown guard. Both the SurfaceView callback and DisposableEffect tear down on the
    // way out, but `nativeClose` frees the handle — so once it's closed, NO path may touch the handle
    // again (use-after-free → SIGSEGV: the consistent back-while-streaming crash). Both run on the
    // main thread, so a plain flag is race-free; AtomicBoolean just makes the intent explicit.
    val closed = remember { AtomicBoolean(false) }
    // The mic opens off the UI thread (AAudio input opens can take hundreds of ms), one start at a
    // time. Every stop bumps `micGen`, so a start that lost its surface meanwhile undoes itself.
    val micStarter = remember {
        Executors.newSingleThreadExecutor { r -> Thread(r, "pf-mic-start").apply { isDaemon = true } }
    }
    val micGen = remember { AtomicInteger(0) }

    // Everything this stream does to the window — wake/Wi-Fi locks, the refresh pin, ALLM, the
    // cutout and soft-keyboard modes, the landscape lock — and the prior values it puts back.
    val streamWindow = remember(handle) {
        StreamWindow(activity, context, composeView, lowLatencyMode, isTv, streamHz)
    }


    DisposableEffect(handle) {
        streamWindow.attach()
        peripherals.start()
        // The panel's refresh pin, unbuffered input dispatch and the render-rate vote.
        streamWindow.pinDisplay()
        onDispose {
            closed.set(true) // from here the handle gets freed; surfaceDestroyed must not touch it
            micGen.incrementAndGet()
            micStarter.shutdown()
            peripherals.stop()
            streamWindow.detach()
            // Leaving the stream: stop the mic + audio + decode threads and tear down the session.
            releaseMicEffects(micEffects)
            NativeBridge.nativeStopMic(handle)
            NativeBridge.nativeStopAudio(handle)
            NativeBridge.nativeStopVideo(handle)
            NativeBridge.nativeVideoDrain(handle, false)
            SessionGate.close(handle) // the QUIC close drains for up to 300 ms, off this thread
        }
    }

    // The twist and the three-finger tap live in the pointer touch models only — passthrough gives
    // every finger to the host verbatim — and need a screen plus the POINTER grant.
    val gestures = hasTouch && touchMode != TouchMode.TOUCH && pointerOk
    // Settings can turn Back off, but only while another opener exists: the ring holds End stream.
    val backOpensRing = initialSettings.backOpensRing || !(ui.padPresent || keyboard || gestures)
    val openRingCentred = { ring.openAt(Offset(containerSize.width / 2f, containerSize.height / 2f)) }
    // The quick-action ring (design/touch-client-overlay.md §2). Back opens it at the screen
    // centre instead of ending the session; "End stream" is a slot inside, behind a two-press arm.
    // Back never falls through: an edge swipe mid-game must not tear the session down.
    BackHandler {
        when {
            activity?.mouseForwarder?.backIsMouseEcho() == true -> {}
            ring.sheet -> ring.sheet = false
            ring.committed -> ring.close()
            backOpensRing -> openRingCentred()
        }
    }
    val hostRecord = remember(session.hostId) {
        session.hostId?.let { id -> KnownHostStore(context).all().firstOrNull { it.id == id } }
    }
    val hostActions by rememberHostActions(handle, hostRecord)
    val streamedGame by rememberStreamedGame(handle, hostRecord, ring.committed)
    val scope = rememberCoroutineScope()

    // The background keep-alive (Settings › General). Off — the default, and what every build
    // before it did — means leaving the app ends the session. Never on a TV: the notification the
    // End action lives on has nowhere to appear there, so a held session would be one nothing
    // outside the app could stop.
    val keepAliveSpan = keepAliveSpanMs(initialSettings, isTv)
    val keepAlive = keepAliveSpan != null

    // Ending from a path that fires while the app is already away — minutes after the recomposer
    // paused, so the disposal that `onSessionEnded` schedules will not run until the user comes
    // back, leaving the host streaming to an empty room. Repeating is free: a stale handle makes
    // every native call here a no-op, and the disposal runs the same ones. `reason` decides where
    // the app lands: `GAME_EXITED` returns a library launch to its own shelf.
    fun endAway(deliberate: Boolean, reason: SessionEndReason = SessionEndReason.LOCAL) {
        if (deliberate) NativeBridge.nativeDisconnectQuit(handle) else NativeBridge.nativeClose(handle)
        StreamKeepAliveService.stop(context)
        onSessionEnded(reason)
    }

    KeepAliveEffects(session, hostRecord, keepAliveSpan, { endAway(it) }, onSessionEnded)

    // Auto-engage pointer capture at stream start (setting on + a mouse actually present).
    // Delayed a beat: the grab needs window focus and the capture view attached.
    LaunchedEffect(handle) {
        delay(400)
        activity?.mouseForwarder?.engageFromStart()
    }

    // A hinge splits the stream (design §4.4): the picture keeps the upper half, the lower one the
    // companion panel, or the pad alone on a fold too shallow for the panel. The stream half is a
    // container like any other — the video fit, the gesture layer, the ring and the hints all
    // measure against it, so none of them knows a fold happened.
    var padSize by remember { mutableStateOf(IntSize.Zero) }
    val split = fold?.takeIf { hingeCompanion || (padShown && !companionUp) }
    // A live mode switch: the asked-for mode shows at once, the host's answer half a second later.
    fun switchMode(w: Int, h: Int, hz: Int) {
        if (NativeBridge.nativeRequestMode(handle, w, h, hz)) {
            requestedMode = intArrayOf(w, h, hz)
            scope.launch {
                delay(500)
                NativeBridge.nativeVideoSize(handle)?.takeIf { it.size >= 3 }?.let { requestedMode = it }
            }
        }
    }
    // A mode the player picked in the sheet this stream, which a swap never changes.
    var modePicked by remember(handle) { mutableStateOf(false) }

    /**
     * What Automatic resolves for layout [l] on this pair, asked when it differs from the live
     * mode: live where the host takes the switch, else from the next connect, which resolves
     * against the same screen ([pictureDisplay]) and the kept layout ([pictureSpanned]).
     */
    fun askModeFor(l: ScreenLayout) {
        val automatic = initialSettings.width <= 0 || initialSettings.height <= 0 || initialSettings.hz <= 0
        if (companionDisplay == null || modePicked || !automatic) return
        // ponytail: the picture screen follows the KEPT layout, so a launch that spans over a
        // kept swap asks at the second screen's width; the next cycle sets it right.
        val (baseW, baseH, hz) = initialSettings.effectiveMode(context, spanned = l == ScreenLayout.SPANNED)
        val (w, h) = RenderScale.apply(
            baseW, baseH, initialSettings.renderScale, RenderScale.maxDimension(initialSettings.codec),
        )
        if (!intArrayOf(w, h, hz).contentEquals(requestedMode)) switchMode(w, h, hz)
    }

    /** The pair's next layout, remembered, and the mode that goes with it. */
    fun cycleScreens() {
        val key = screenKey ?: return
        layoutPick = layout.next(layoutsOffered)
        CompanionMemory.keepLayout(context, key, layoutPick)
        runCatching { NativeBridge.nativeLogDisplay("screens: ${layoutPick.name.lowercase()} $key") }
        askModeFor(layoutPick)
    }
    // A layout the connect did not ask for — a two-screen console's launch — asks now.
    LaunchedEffect(handle, screenKey) {
        if (screenKey != null && layout != CompanionMemory.layout(context, screenKey)) askModeFor(layout)
    }
    // Which picture layers show: the second window's alone with the picture below on a second
    // display, both when it spans, else the activity's.
    LaunchedEffect(handle, layout, companionDisplay?.displayId) {
        NativeBridge.nativePictureShown(
            handle,
            when {
                spanned -> NativeBridge.PICTURE_FIRST or NativeBridge.PICTURE_SECOND
                layout == ScreenLayout.SWAPPED && companionDisplay != null -> NativeBridge.PICTURE_SECOND
                else -> NativeBridge.PICTURE_FIRST
            },
        )
    }
    // What the ring's slots and the companion's action tiles do this session.
    val ringActions = RingActions(
        endStream = { NativeBridge.nativeDisconnectQuit(handle); onSessionEnded(SessionEndReason.LOCAL) },
        disconnectLinger = { onSessionEnded(SessionEndReason.LOCAL) },
        touchMode = { touchMode },
        cycleTouchMode = {
            // Passthrough is skipped toward a host that drops contacts (§5.4).
            val order = if (hostAcceptsTouch) TouchMode.entries else TouchMode.entries - TouchMode.TOUCH
            touchMode = order[(order.indexOf(touchMode) + 1) % order.size]
        },
        keyboardGranted = { ui.accessGrants and SessionAccess.KEYBOARD != 0 },
        keyboard = { keyCapture?.setImeVisible(true) },
        textSupported = NativeBridge.nativeTextInputSupported(handle),
        sendText = { NativeBridge.nativeSendText(handle, it) },
        stats = { ui.statsVerbosity },
        cycleStats = { ui.statsVerbosity = ui.statsVerbosity.next() },
        micAvailable = { ui.micRunning },
        micMuted = { ui.micMuted },
        toggleMic = { ui.mute(!ui.micMuted) },
        hostActions = { hostActions },
        invokeHost = { act ->
            hostRecord?.let { kh ->
                scope.launch(Dispatchers.IO) {
                    (IdentityStore(context).load() as? IdentityLoad.Ok)?.identity?.let { id ->
                        HostActions.invoke(id, kh.address, kh.effectiveMgmtPort, kh.fpHex, kh.name, act.id, act.label)
                    }
                }
            }
        },
        sendShortcut = { sendChord(handle, it) },
        padAvailable = { activity?.gamepadRouter?.sendsEnabled() == true },
        padShown = { padShown },
        togglePad = { padShown = !padShown },
        tapPadButton = { bit -> activity?.gamepadRouter?.tapButton(bit) },
        pointerGranted = { ui.accessGrants and SessionAccess.POINTER != 0 },
        padMouseTarget = { padMouseTarget(ring, activity?.gamepadRouter) },
        padMouseMode = {
            NativeBridge.nativePadMouseMode(handle, padMouseTarget(ring, activity?.gamepadRouter))
        },
        cyclePadMouse = {
            NativeBridge.nativeCyclePadMouse(handle, padMouseTarget(ring, activity?.gamepadRouter))
        },
        audioMute = { ui.audioMute },
        audioMuteLabel = { ui.audioMuteLabel },
        toggleStreamMute = { ui.muteStream(!ui.streamMuted) },
        scrollInverted = { ui.invertScroll },
        toggleScrollInversion = { ui.setScrollInverted(!ui.invertScroll) },
        streamedGame = { streamedGame },
        endGame = {
            val game = streamedGame
            val appId = game?.appId
            val kh = hostRecord
            if (game != null && appId != null && kh != null) {
                scope.launch {
                    val outcome = withContext(Dispatchers.IO) {
                        (IdentityStore(context).load() as? IdentityLoad.Ok)?.identity?.let { id ->
                            LibraryClient.endGame(
                                kh.address, kh.effectiveMgmtPort, id.certPem, id.privateKeyPem,
                                kh.fpHex, appId,
                            )
                        } ?: GameEnd.Failed("this device has no identity yet")
                    }
                    // Gone either way: leave as End stream does. A refusal keeps the stream.
                    if (outcome.gameGone) {
                        NativeBridge.nativeDisconnectQuit(handle)
                        onSessionEnded(SessionEndReason.LOCAL)
                    } else {
                        Toast.makeText(context, outcome.notice(game.title), Toast.LENGTH_LONG).show()
                    }
                }
            }
        },
        currentMode = { requestedMode },
        requestMode = { w, h, hz ->
            modePicked = true
            switchMode(w, h, hz)
        },
        screenLayouts = { layoutsOffered },
        screenLayout = { layout },
        cycleScreens = ::cycleScreens,
    )
    // The summon rides a pointer gesture but TYPES, so it also needs the KEYBOARD grant
    // (dismissing is always allowed).
    val showKeyboard: (Boolean) -> Unit = { show ->
        if (!show || ui.accessGrants and SessionAccess.KEYBOARD != 0) keyCapture?.setImeVisible(show)
    }
    val companion: @Composable () -> Unit = {
        CompanionPanel(
            pages = companionPages,
            page = companionPage,
            onPage = { companionPick = it; CompanionMemory.keep(context, it) },
            header = PanelHeader(
                title = listOfNotNull(hostRecord?.name, streamedGame?.title ?: session.launchHold?.game?.title)
                    .joinToString(" · ").ifEmpty { "Punktfunk" },
                detail = requestedMode.takeIf { it.size >= 3 && it[0] > 0 }
                    ?.let { "${it[0]}×${it[1]} · ${it[2]} Hz" }.orEmpty(),
            ),
            stats = statsLines,
            tier = hudTier,
            onTier = { ui.statsVerbosity = it },
            cfg = overlayCfg,
            actions = ringActions,
            haptics = haptics,
            keys = KeySink { vk, down -> NativeBridge.nativeSendKey(handle, vk, down, 0) },
            trackpad = {
                streamTouchInput(
                    NativeTouchSink(handle), null, ::videoFrame, trackpad = true,
                    onCycleStats = { ui.statsVerbosity = ui.statsVerbosity.next() },
                    onKeyboard = showKeyboard,
                    onDial = {},
                )
            },
            pad = { size -> PadHalf(virtualPad, overlayCfg.pad, size, haptics, openRingCentred) },
        )
    }
    // A safe-area mode asked the host for a picture narrower than the panel by the housing, so the
    // box it lands in is the window's live cutout-safe width: it follows a flip to the other
    // landscape. Left and right are physical sides, so an RTL layout cannot swap them.
    val safe = initialSettings.width == SAFE_AREA_MODE
    // A surface laid out at the frame's placement: MediaCodec scales whatever it decodes to fill
    // the Surface, so the SurfaceView carries the picture's shape, and native crops the source to
    // the part that stays visible (Crop to fill).
    fun videoRect(frameMap: FrameMap): Modifier = if (frameMap.isEmpty) {
        Modifier.fillMaxSize()
    } else {
        val place = frameMap.placement
        Modifier
            .offset { IntOffset(place.dstX, place.dstY) }
            .layout { measurable, _ ->
                val p = measurable.measure(Constraints.fixed(place.dstW, place.dstH))
                layout(place.dstW, place.dstH) { p.place(0, 0) }
            }
    }
    // The decoder's window: the activity's video SurfaceView, created once per stream. Movable
    // content, so the picture can take a hinge's lower half and come back without the surface —
    // and with it the decoder — being recreated.
    val videoSurface = remember(handle) {
        movableContentOf { frameMap: FrameMap ->
            LaunchedEffect(handle, frameMap) {
                val c = frameMap.sourceCrop()
                NativeBridge.nativeVideoSourceCrop(handle, c[0], c[1], c[2], c[3])
            }
            AndroidView(
                modifier = videoRect(frameMap),
                factory = { ctx ->
                    SurfaceView(ctx).apply {
                        videoView = this
                        holder.addCallback(object : SurfaceHolder.Callback {
                            override fun surfaceCreated(holder: SurfaceHolder) {
                                // The keep-alive's drain, if one is running: the decode thread is
                                // about to take the frame queue back.
                                NativeBridge.nativeVideoDrain(handle, false)
                                // Low-latency mode: rank MediaCodecList decoders for the negotiated
                                // MIME (framework-only API) and hand the chosen one to Rust, which
                                // creates it by name and applies the per-SoC vendor low-latency keys.
                                // Off ⇒ no ranking: the platform resolves its default decoder for the
                                // MIME, exactly as before the overhaul.
                                val mime = NativeBridge.nativeVideoMime(handle)
                                val choice = if (lowLatencyMode) VideoDecoders.pickDecoder(mime) else null
                                NativeBridge.nativeStartVideo(
                                    handle,
                                    holder.surface,
                                    choice?.name ?: "",
                                    lowLatencyMode,
                                    choice?.lowLatencyFeature ?: false,
                                    isTv,
                                    isChromeOs,
                                    initialSettings.presentPriorityWire(),
                                    initialSettings.smoothBuffer,
                                    // The refresh of the panel this view is on — from the mode TABLE
                                    // (streamPanelFps), because display.refreshRate reports a per-uid
                                    // override, not the panel. Fallback: the (possibly lying) live rate.
                                    this@apply.display?.streamPanelFps(streamHz)?.takeIf { it > 0 }
                                        ?: (this@apply.display?.refreshRate ?: 0f).roundToInt(),
                                    // The SurfaceView's on-screen pixel size — the coordinate space the
                                    // ASurfaceControl layer composites in (the aspect-fitted video rect,
                                    // not the window's rotated buffer geometry). 0 if not laid out yet;
                                    // native falls back to the window buffer size.
                                    this@apply.width,
                                    this@apply.height,
                                )
                                NativeBridge.nativeStartAudio(handle, lowLatencyMode, isTv)
                                // The MIC grant is read live (a surface recreate re-runs this, and
                                // the mask may have changed since the last one): without it no
                                // capture opens — the host never attached this session to its mic
                                // service, so the platform's recording indicator would announce a
                                // mic nobody can hear.
                                if (micWanted && ui.accessGrants and SessionAccess.MIC != 0) {
                                    val gen = micGen.incrementAndGet()
                                    val echo = initialSettings.echoCancel
                                    val main = ContextCompat.getMainExecutor(context)
                                    if (!micStarter.isShutdown) micStarter.execute {
                                        if (micGen.get() != gen) return@execute
                                        val sessionId = NativeBridge.nativeStartMic(handle, echo)
                                        // Stopped during the open: that stop found nothing to stop.
                                        if (micGen.get() != gen) {
                                            NativeBridge.nativeStopMic(handle)
                                            return@execute
                                        }
                                        main.execute {
                                            if (micGen.get() != gen) return@execute
                                            if (ui.accessGrants and SessionAccess.MIC == 0) {
                                                NativeBridge.nativeStopMic(handle) // revoked meanwhile
                                                return@execute
                                            }
                                            if (echo) attachMicEffects(sessionId, micEffects)
                                            // Did a capture actually open? That — not the setting —
                                            // puts the mute control on screen. A restart after a
                                            // surface recreate comes back already muted if the user
                                            // muted: the flag lives on the session handle.
                                            ui.micRunning = NativeBridge.nativeMicActive(handle)
                                        }
                                    }
                                }
                            }

                            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
                                // The view's CURRENT pixel size, for the ASurfaceControl layer's
                                // destination rect. It is reported here and not only at
                                // surfaceCreated because the view grows a frame or two after the
                                // stream screen appears — hiding the system bars and switching on
                                // cutout drawing both resize it, and neither recreates the surface.
                                // A layer left on the start-up rect paints the picture small, in the
                                // top-left corner. The view's own size, not the buffer geometry in
                                // `width`/`height`: the layer composites in the view's space.
                                NativeBridge.nativeVideoSurfaceSize(
                                    handle, this@apply.width, this@apply.height,
                                )
                                // Re-assert the frame-rate vote: a buffer-geometry change can reset
                                // the surface's frame-rate setting on some OEM builds, silently
                                // dropping the 120 Hz pin mid-stream. Mirrors the native hint's
                                // policy (FIXED_SOURCE; ALWAYS only on the TV low-latency path —
                                // phones stay seamless so a re-hint can never force a mode flicker).
                                if (streamHz > 0) runCatching {
                                    holder.surface.setFrameRate(
                                        streamHz.toFloat(),
                                        Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE,
                                        if (isTv && lowLatencyMode) {
                                            Surface.CHANGE_FRAME_RATE_ALWAYS
                                        } else {
                                            Surface.CHANGE_FRAME_RATE_ONLY_IF_SEAMLESS
                                        },
                                    )
                                }
                            }

                            override fun surfaceDestroyed(holder: SurfaceHolder) {
                                // Surface gone (backgrounding, or on the way out). Stop the threads that
                                // render to it — but only while the session is still open. Once
                                // DisposableEffect has closed it, the handle is freed; dereferencing it
                                // here is the use-after-free that crashed on back-navigation.
                                if (!closed.get()) {
                                    micGen.incrementAndGet()
                                    releaseMicEffects(micEffects)
                                    NativeBridge.nativeStopMic(handle)
                                    // No capture, no control — but the MUTE state is deliberately left
                                    // standing (native keeps it on the handle), so the restart in
                                    // surfaceCreated brings the user's choice back with it.
                                    ui.micRunning = false
                                    // Audio is the one plane the keep-alive does NOT stop — the
                                    // sound carrying on is the whole point of it. Video stops
                                    // either way (its Surface is gone), but with the session held
                                    // something must keep popping access units, or the queue
                                    // stands and the client asks the host for a keyframe every
                                    // two seconds until the user comes back.
                                    if (!keepAlive) NativeBridge.nativeStopAudio(handle)
                                    NativeBridge.nativeStopVideo(handle)
                                    if (keepAlive) NativeBridge.nativeVideoDrain(handle, true)
                                }
                            }
                        })
                    }
                },
            )
        }
    }
    // The second picture window's surface, for the presenter's second layer (design §4): the
    // picture on a second display, or its lower half there or below a hinge.
    val pictureSurface: @Composable (FrameMap) -> Unit = { frameMap ->
        LaunchedEffect(handle, frameMap) {
            val c = frameMap.sourceCrop()
            NativeBridge.nativePictureCrop(handle, c[0], c[1], c[2], c[3])
        }
        PictureSurface(handle, videoRect(frameMap))
    }
    // The row the two screens split a spanned picture at: half for a second display (the mode
    // is two equal halves), the hinge's share of the panel on a fold.
    fun splitRow(): Int {
        val h = videoFrame().height
        val s = split?.takeIf { companionDisplay == null } ?: return h / 2
        val panel = (rootSize.height - s.hingePx).coerceAtLeast(1)
        return (h.toLong() * s.videoPx / panel).toInt().coerceIn(0, h)
    }
    fun upperFrame(): VideoFrame =
        videoFrame().let { if (spanned) VideoFrame(it.fit, it.width, it.height, 0, splitRow()) else it }
    fun lowerFrame(): VideoFrame =
        videoFrame().let { val top = splitRow(); VideoFrame(it.fit, it.width, it.height, top, it.height - top) }
    /**
     * The picture in one box: [surface] at [frame]'s placement, the gesture layer over it, and
     * with [chrome] everything else drawn on it or read off it — the HUD and hints, the pad, the
     * ring. [frame] is the whole picture, or the half this box shows when it spans two screens.
     * The box with the chrome is the one the mouse and the ring measure against.
     */
    val picture: @Composable (Modifier, () -> VideoFrame, @Composable (FrameMap) -> Unit, Boolean) -> Unit =
        { modifier, frame, surface, chrome ->
            var size by remember { mutableStateOf(IntSize.Zero) }
            Box(
                modifier = modifier
                    .then(
                        if (safe) {
                            Modifier.windowInsetsPadding(WindowInsets.displayCutout.only(WindowInsetsSides.Horizontal))
                        } else {
                            Modifier
                        },
                    )
                    .onSizeChanged {
                        size = it
                        if (chrome) containerSize = it
                    },
            ) {
                surface(frame().at(size))
                if (chrome) {
                    // Live stats HUD (FPS / throughput / capture→client latency), drawn over the video
                    // but BEFORE the transparent gesture layer below, so it shows through and never
                    // eats touches. A companion panel carries it instead.
                    val statsShown = !companionUp && statsOn && statsLines.isNotEmpty()
                    val statsCorner = hudAlignment(initialSettings.hudPlacement)
                    if (statsShown) {
                        val placement = Modifier.align(statsCorner).padding(12.dp)
                        OsdScaled { StatsOverlay(statsLines, placement, initialSettings.statsScalePct / 100f) }
                    }
                    // The Access chip — what this session is allowed to do, said in the preset
                    // vocabulary ("Controller only · 1 h 58 m left"), shown while the stats HUD is on.
                    // It rides the stats tier rather than standing for the whole stream: a pill that
                    // never goes away is chrome you read as distraction. Full control with no expiry —
                    // every session against an old host, and most against a new one — shows NOTHING:
                    // the chip exists for the sessions where input silently not landing needs an
                    // explanation, not as new chrome on everyone's stream. TopEnd, in the shared pill
                    // family (TopStart is the HUD's, TopCentre the transient cues', BottomCentre the
                    // banner's).
                    val accessChip = when {
                        ui.statsVerbosity == StatsVerbosity.OFF -> null
                        ui.accessGrants and SessionAccess.ALL == SessionAccess.ALL && ui.accessRemaining == 0 -> null
                        ui.accessRemaining > 0 ->
                            "${SessionAccess.label(ui.accessGrants)} · " +
                                "${SessionAccess.remainingLabel(ui.accessRemaining)} left"
                        else -> SessionAccess.label(ui.accessGrants)
                    }
                    // Same corner, stacked: the mute sentence stands whatever the stats tier, because
                    // a player who cannot hear is owed the reason even with chrome off. Top left while
                    // the stats panel holds the top right.
                    if (accessChip != null || ui.audioMuteLabel != null) {
                        val left = statsShown && statsCorner == Alignment.TopEnd
                        OsdScaled {
                            Column(
                                Modifier.align(if (left) Alignment.TopStart else Alignment.TopEnd).padding(12.dp),
                                verticalArrangement = Arrangement.spacedBy(8.dp),
                                horizontalAlignment = if (left) Alignment.Start else Alignment.End,
                            ) {
                                ui.audioMuteLabel?.let { AccessChip(it) }
                                accessChip?.let { AccessChip(it) }
                            }
                        }
                    }
                    // "Hold to quit" hint while the gamepad exit chord is armed — the exit debounces
                    // on a ~1 s hold, so without this cue a couch user reads the (deliberately
                    // no-longer-instant) chord as broken. Purely visual; it sits above the video and
                    // below the gesture layer.
                    if (ui.exitArming) {
                        OsdScaled { ExitChordHint(Modifier.align(Alignment.TopCenter).padding(top = 16.dp)) }
                    }
                    // Remote-pointer mode hint — the remote's keys are remapped while it's on, so say so.
                    if (ui.remotePointerOn) {
                        OsdScaled { RemotePointerHint(Modifier.align(Alignment.TopCenter).padding(top = 16.dp)) }
                    }
                    // The exit hint (desktop parity): one line on how to leave with the input in hand,
                    // the pad chord when a controller is here. Without one, leaving is a slot in the
                    // quick-action dial, so the line names what opens it. Recomputed rather than
                    // captured: a pad can wake mid-hint. Above the video and below the gesture layer,
                    // so it never eats a touch.
                    //
                    // Bottom-centre, which MotionUnreachableHint also owns at t≈0. The hint YIELDS: the
                    // notice reports something broken about THIS session, the hint repeats every stream.
                    if (initialSettings.exitHint && banner.up && !ui.motionHint && !touchHint) OsdScaled {
                        StreamStartBanner(
                            text = when {
                                ui.padPresent -> "Hold L1 + R1 + Start + Select to leave"
                                backOpensRing -> "Back opens quick actions"
                                gestures -> "A two-finger twist opens quick actions"
                                else -> "Ctrl+Alt+Shift+O opens quick actions"
                            },
                            alpha = banner.alpha,
                            modifier = Modifier.align(Alignment.BottomCenter).padding(bottom = 24.dp),
                        )
                    }
                }
                // Touch input per the Settings model: trackpad/direct-pointer mouse (the shared gesture
                // vocabulary), the same gestures with nothing sent (Off, so a miss beside the pad stays
                // put), or real multi-touch passthrough — see TouchInput.kt. Passthrough gets no
                // keyboard gesture: its fingers belong to the host verbatim (a swipe there may BE a
                // host-OS gesture), so intercepting three fingers would corrupt real multi-touch.
                // Stylus lane (design/pen-tablet-input.md §7): against a HOST_CAP_PEN host a stylus
                // splits out of BOTH touch models onto the pen plane; its heartbeat coroutine keeps a
                // stationary held stroke alive (and its cancellation lifts everything on teardown).
                // The POINTER grant gates the whole touch/stylus capture layer — "don't capture what
                // can't land": ungranted, no gesture handler is installed at all (and no pen lane opens),
                // rather than fingers being read into events the host will drop. Keyed on the grant so an
                // AccessUpdate flipping it mid-session swaps the layer live.
                val stylus = remember(handle, pointerOk) {
                    if (pointerOk && NativeBridge.nativeHostSupportsPen(handle)) StylusStream(handle) else null
                }
                if (stylus != null) {
                    LaunchedEffect(stylus) { stylus.heartbeatLoop() }
                }
                Box(
                    Modifier.fillMaxSize().pointerInput(handle, touchMode, pointerOk) {
                        when {
                            !pointerOk -> {} // no capture — the Access chip is what says why
                            touchMode == TouchMode.TOUCH ->
                                streamTouchPassthrough(NativeTouchSink(handle), stylus, frame)
                            else -> streamTouchInput(
                                if (touchMode == TouchMode.OFF) DroppedTouchSink else NativeTouchSink(handle),
                                stylus,
                                frame,
                                trackpad = touchMode != TouchMode.POINTER,
                                onCycleStats = { ui.statsVerbosity = ui.statsVerbosity.next() },
                                onKeyboard = showKeyboard,
                                // The two-finger twist turns the quick-action ring, frame by frame.
                                onDial = { ev ->
                                    when (ev) {
                                        is DialEvent.Turn ->
                                            if (ring.turn(ev.progress, ev.clockwise, ev.x, ev.y)) haptics.tick()
                                        DialEvent.Commit -> { ring.commit(); haptics.confirm() }
                                        DialEvent.Cancel -> ring.cancel()
                                    }
                                },
                            )
                        }
                    },
                )
                if (chrome) {
                    // No standing mic element here: the in-stream control is deliberately absent until
                    // the on-screen overlay UI lands and can carry it as one of its controls. Mute
                    // itself is intact — the Select + Y chord toggles it, and the hint below is what
                    // confirms the toggle: a toggle that showed nothing at all would be
                    // indistinguishable from one that never registered.
                    // The virtual controller: above the gesture layer, so its controls take their
                    // fingers first and every other finger falls through; below the ring, whose scrim
                    // owns every finger while it is up. Composed only while shown (tenet 1) — and with
                    // a lower half or a companion panel it leaves this half entirely for that.
                    if (split == null && !companionUp) PadHalf(virtualPad, overlayCfg.pad, containerSize, haptics, openRingCentred)
                    // The ring, above the gesture layer so its buttons take the finger first. Composed
                    // only while open: a closed overlay costs nothing (tenet 1).
                    OsdScaled {
                        RingOverlay(
                            state = ring,
                            cfg = overlayCfg,
                            actions = ringActions,
                            containerSize = containerSize,
                            haptics = haptics,
                        )
                    }
                    ui.micHint?.let {
                        OsdScaled { MicChordHint(it, Modifier.align(Alignment.TopCenter).padding(top = 16.dp)) }
                    }
                    // Bottom, not top: this can coincide with a mic-chord confirmation or the exit cue,
                    // and a notice landing on top of one of those would cost the user both.
                    OsdScaled {
                        if (ui.motionHint) {
                            MotionUnreachableHint(Modifier.align(Alignment.BottomCenter).padding(bottom = 24.dp))
                        } else if (touchHint) {
                            TouchFallbackHint(Modifier.align(Alignment.BottomCenter).padding(bottom = 24.dp))
                        }
                    }
                }
            }
        }
    Box(Modifier.fillMaxSize().background(Color.Black).onSizeChanged { rootSize = it }) {
        // Invisible 1-px focus anchor for the host-typing soft keyboard (three-finger swipe up in
        // the mouse modes) AND the pointer-capture grab target. It never draws or takes touches,
        // and it stays in the activity's window wherever the picture is: that window keeps focus.
        AndroidView(
            modifier = Modifier.size(1.dp),
            factory = { ctx ->
                KeyCaptureView(ctx).also { v ->
                    keyCapture = v
                    // Real IME text path when the host types committed text (see KeyCaptureView).
                    v.textHandle =
                        if (NativeBridge.nativeTextInputSupported(handle)) handle else 0L
                    v.setOnCapturedPointerListener { _, ev ->
                        (ctx as? MainActivity)?.mouseForwarder?.onCapturedPointer(ev) ?: false
                    }
                    v.preIme = { ev -> (ctx as? MainActivity)?.streamKey(ev) == true }
                }
            },
        )
        Column(Modifier.fillMaxSize()) {
            val upper = Modifier.fillMaxWidth().then(
                if (split != null) Modifier.height(with(density) { split.videoPx.toDp() }) else Modifier.weight(1f),
            )
            when {
                // The picture below on a second display: the decoder's window stays in this one,
                // hidden under the panel, so nothing restarts.
                layout == ScreenLayout.SWAPPED && companionDisplay != null -> Box(upper) {
                    videoSurface(VideoFrame(videoFit, 0, 0).at(IntSize.Zero))
                    companion()
                }
                layout == ScreenLayout.SWAPPED -> Box(upper) { companion() }
                else -> picture(upper, ::upperFrame, videoSurface, true)
            }
            if (split != null) {
                // The hinge itself: nothing on a creased panel, a real strip on a two-panel device.
                Spacer(Modifier.height(with(density) { split.hingePx.toDp() }))
                Box(modifier = Modifier.fillMaxWidth().weight(1f).onSizeChanged { padSize = it }) {
                    when {
                        !hingeCompanion -> PadHalf(virtualPad, overlayCfg.pad, padSize, haptics, openRingCentred)
                        layout == ScreenLayout.SWAPPED -> picture(Modifier.fillMaxSize(), ::videoFrame, videoSurface, true)
                        spanned -> picture(Modifier.fillMaxSize(), ::lowerFrame, pictureSurface, false)
                        else -> companion()
                    }
                }
            }
            companionDisplay?.let {
                when (layout) {
                    ScreenLayout.SWAPPED -> CompanionOnDisplay(it, pictureHz = streamHz) {
                        picture(Modifier.fillMaxSize(), ::videoFrame, pictureSurface, true)
                    }
                    // The lower half keeps the panel's own rate: it is the touch screen under a game.
                    ScreenLayout.SPANNED -> CompanionOnDisplay(it, pictureHz = 0) {
                        picture(Modifier.fillMaxSize(), ::lowerFrame, pictureSurface, false)
                    }
                    ScreenLayout.PANEL -> CompanionOnDisplay(it, content = companion)
                }
            }
            // Last, so it covers everything: the launched title's poster until its game is up.
            var launchHold by remember(session) { mutableStateOf(session.launchHold) }
            launchHold?.let {
                LaunchHoldOverlay(
                    it,
                    // Retry is the shelf the launch came off: ending as `GAME_EXITED` puts a library
                    // launch back on it, one press from the same tile.
                    onRetry = { endAway(true, SessionEndReason.GAME_EXITED) },
                    onShow = { launchHold = null },
                )
            }
        }
    }
}

/**
 * The virtual controller wherever it is held: overlaid on the picture, alone on the lower half of a
 * fold, or on the companion's controller page. The wire pad itself lives in [rememberVirtualPad],
 * so moving the layer never makes the host see a controller reconnect.
 */
@Composable
private fun PadHalf(pad: GamepadRouter.ExternalPad?, cfg: PadConfig, size: IntSize, haptics: ConsoleHaptics, openRing: () -> Unit) {
    if (pad == null) return
    val ring by rememberUpdatedState(openRing)
    val sink = remember(pad) { PadSink(pad::button, pad::axis) { ring() } }
    VirtualPadLayer(cfg, size, sink, haptics)
}

/**
 * "This host doesn't accept touch" — shown briefly when the Touch (passthrough) model meets a host
 * whose injector drops contacts (no `HOST_CAP2_TOUCH`). The session runs the trackpad model
 * instead; without this line the user would see their setting silently ignored.
 */
@Composable
private fun TouchFallbackHint(modifier: Modifier = Modifier) {
    Text(
        "This host doesn't accept touch — using the trackpad model",
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 14.dp, vertical = 8.dp),
        color = Color.White,
        fontSize = 15.sp,
    )
}

/**
 * Attach the Java echo-canceller + noise-suppressor pair to the mic stream's audio session — the
 * backstop for HALs whose VoiceCommunication capture path doesn't cancel on its own (the native
 * side already opened the stream under that preset). [sessionId] `<= 0` means native allocated no
 * session (echo cancellation off, or the preset fell back to the plain open), so there is nothing
 * to hang an effect on. Created effects land in [into] for [releaseMicEffects]; `create()`
 * returning null (unsupported / claimed) is quietly nothing — the HAL preset still does its part.
 * Needs no extra permission: the effect APIs attach to our own recording session.
 */
/**
 * Engage a USB capture on [dev], asking the user for access first when we don't already hold it.
 *
 * Returns the receiver left waiting on that grant — the caller unregisters it on teardown — or null
 * when [start] has already run, which is the common case: Android remembers a grant for as long as
 * the device stays attached, so the dialog appears once per plug-in and never mid-stream after that.
 *
 * Shared by the Steam Controller 2 and Sony captures, whose bring-up differed only in the broadcast
 * action, the [requestCode] and the wording of the denial. Two copies of a permission handshake is
 * one copy too many: a fix to either — and the fix that made the grant intent MUTABLE was one —
 * has to be found and made twice.
 */
internal fun requestUsbCapture(
    context: Context,
    dev: UsbDevice,
    action: String,
    /** Distinct per capture: PendingIntents with equal request codes and actions collide. */
    requestCode: Int,
    /** What the denial is called in the log — the pad the user just refused. */
    label: String,
    start: (UsbDevice) -> Unit,
): BroadcastReceiver? {
    val usb = context.getSystemService(Context.USB_SERVICE) as UsbManager
    if (usb.hasPermission(dev)) {
        start(dev)
        return null
    }
    val receiver = object : BroadcastReceiver() {
        override fun onReceive(c: Context?, intent: Intent?) {
            if (intent?.action != action) return
            val ok = intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)
            if (ok) start(dev) else Log.i("punktfunk", "$label USB permission denied")
        }
    }
    ContextCompat.registerReceiver(
        context, receiver, IntentFilter(action), ContextCompat.RECEIVER_NOT_EXPORTED,
    )
    usb.requestPermission(
        dev,
        PendingIntent.getBroadcast(
            context, requestCode,
            Intent(action).setPackage(context.packageName),
            // MUTABLE: the USB stack appends the grant extras to this intent.
            PendingIntent.FLAG_MUTABLE,
        ),
    )
    return receiver
}

private fun attachMicEffects(sessionId: Int, into: MutableList<AudioEffect>) {
    if (sessionId <= 0) return
    if (AcousticEchoCanceler.isAvailable()) {
        AcousticEchoCanceler.create(sessionId)?.let { it.setEnabled(true); into.add(it) }
    }
    if (NoiseSuppressor.isAvailable()) {
        NoiseSuppressor.create(sessionId)?.let { it.setEnabled(true); into.add(it) }
    }
}

/** Release every attached mic effect engine. Idempotent — the list is cleared, and both stop
 * paths (surface teardown, final dispose) may call it in either order. */
internal fun releaseMicEffects(effects: MutableList<AudioEffect>) {
    effects.forEach { runCatching { it.release() } }
    effects.clear()
}

/**
 * Transient confirmation that the mic chord (Select + Y) registered. Nothing else on screen says
 * *muted* or *un*muted, so this pill carries both — "did that press do anything?" is the whole
 * doubt a chord with no button under the finger creates. Same pill vocabulary as the other
 * in-stream cues; the caller clears it after a beat.
 */
@Composable
private fun MicChordHint(text: String, modifier: Modifier = Modifier) {
    Text(
        text,
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 14.dp, vertical = 8.dp),
        color = Color.White,
        fontSize = 15.sp,
    )
}

/**
 * The standing Access chip — the session's access level in the preset vocabulary, with the live
 * countdown when the grant expires ("Controller only · 1 h 58 m left"). Same pill family as the
 * other in-stream overlays, sized down a step because it stands for the whole session rather than
 * flashing a moment's confirmation. Only composed when there is something to say: a full-control
 * permanent session — today's normal — shows nothing at all.
 */
@Composable
private fun AccessChip(text: String, modifier: Modifier = Modifier) {
    Text(
        text,
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 10.dp, vertical = 5.dp),
        color = Color.White,
        fontSize = 12.sp,
    )
}

/**
 * "This pad's gyro can't reach the game" — shown briefly when a captured controller with motion
 * meets a session whose virtual pad has no motion plane (the X-Box classes have no gyro in their
 * HID contract, so every sample would be decoded and dropped host-side).
 *
 * It names the setting because that is the whole point: without it the player has a gyro that
 * silently does nothing and no way to tell that from a broken sensor. Not a control — the setting
 * applies from the next session, so offering to change it here would promise something this stream
 * cannot deliver. [GamepadRouter.onMotionUnreachable] raises it.
 */
@Composable
private fun MotionUnreachableHint(modifier: Modifier = Modifier) {
    Text(
        "Motion won't reach this session — set Controller type to DualSense",
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 14.dp, vertical = 8.dp),
        color = Color.White,
        fontSize = 15.sp,
    )
}

/**
 * The "hold to quit" cue shown while the gamepad exit chord (Select + Start + L1 + R1) is held. The
 * chord no longer quits on a quick press — the router debounces it on a ~1 s hold — so this confirms
 * the press registered and tells the user to keep holding. Purely visual; [GamepadRouter.onExitArmed]
 * toggles its visibility.
 */
@Composable
private fun ExitChordHint(modifier: Modifier = Modifier) {
    Text(
        "Hold to quit…",
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 14.dp, vertical = 8.dp),
        color = Color.White,
        fontSize = 15.sp,
    )
}

/**
 * The remote-pointer mode cue: while active the remote's keys are remapped (D-pad glides the host
 * cursor, SELECT clicks), so the overlay both confirms the toggle and teaches the vocabulary.
 */
@Composable
private fun RemotePointerHint(modifier: Modifier = Modifier) {
    Text(
        "Remote pointer — SELECT click · play/pause right-click · hold SELECT to exit",
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 14.dp, vertical = 8.dp),
        color = Color.White,
        fontSize = 15.sp,
    )
}

/**
 * The start-of-stream banner: the shortcuts this session actually has, in the same pill as every
 * other in-stream cue, shown once and then gone. The desktop console draws the identical thing
 * bottom-centre (`pf-console-ui/src/skia_overlay.rs` — six seconds with a 0.6 s fade), because a
 * stream owns the whole screen and answers to none of the device's usual gestures: without a line
 * saying how to get back out, the only discoverable exit is force-quitting the app.
 *
 * [text] and [alpha] are the caller's. Only it knows what this session HAS — a pad, a mic, a
 * touchscreen — and only it owns the timer, which is precisely what a screenshot wants to skip.
 * Purely visual: it sits below the gesture layer, takes no touches and is never clickable. Internal
 * so the screenshot scene can shoot the real pill instead of a copy of it that drifts.
 */
@Composable
internal fun StreamStartBanner(text: String, alpha: Float, modifier: Modifier = Modifier) {
    Text(
        text,
        // Alpha FIRST: the fade has to take the pill's backdrop with it, and everything after this
        // in the chain draws inside the layer it opens.
        modifier = modifier
            .alpha(alpha)
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 14.dp, vertical = 8.dp),
        color = Color.White,
        fontSize = 15.sp,
    )
}

/**
 * Invisible focus anchor for typing on the host: the three-finger swipe summons the device IME
 * onto this view. Two IME models, picked by the host's capabilities:
 *  * **Text path** ([textHandle] set — the host advertised `HOST_CAP_TEXT_INPUT`): a real
 *    editable [HostTextConnection], so the IME gives autocorrect, gesture typing, non-Latin
 *    composition and emoji, all mirrored to the host as committed text + diffs.
 *  * **Fallback** (older host): `TYPE_NULL` puts the IME in "dumb keyboard" mode — raw
 *    [KeyEvent]s flow through `MainActivity.dispatchKeyEvent` → `Keymap.toVk` → the host, the
 *    exact path a hardware keyboard takes (with the IME-shift wrap documented there).
 *
 * Doubles as the pointer-capture grab target: a grab needs a focusable view, and captured-pointer
 * events are delivered to it (routed to [MouseForwarder.onCapturedPointer] via the listener the
 * stream screen installs).
 */
internal class KeyCaptureView(context: Context) : View(context) {
    init {
        isFocusable = true
        isFocusableInTouchMode = true
    }

    // A leaf keeps its unbuffered request; a ViewGroup's is recomputed whenever focus moves below
    // it. Pointer classes rise from any child, the rest through this view while it holds focus.
    override fun onAttachedToWindow() {
        super.onAttachedToWindow()
        if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.R) {
            requestUnbufferedDispatch(STREAM_UNBUFFERED_SOURCES)
        }
    }

    override fun onDetachedFromWindow() {
        if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.R) {
            requestUnbufferedDispatch(0)
        }
        super.onDetachedFromWindow()
    }

    /** The session handle when the host types committed text; `0` = VK-only fallback. */
    var textHandle: Long = 0L

    /** Whether [setImeVisible] last showed the IME — for toggle-style callers (remote pointer). */
    var imeShown = false
        private set

    override fun onCheckIsTextEditor(): Boolean = imeShown

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
        // Only an editor while the user has SUMMONED the keyboard (gesture / remote toggle).
        // This view holds focus for the whole stream (it's the capture anchor), and with an
        // always-live editable connection the IME counts input as active on it — TV IMEs then
        // pop their UI the moment a PHYSICAL keyboard key arrives. With no connection, hardware
        // typing stays on the raw dispatchKeyEvent → Keymap → wire path and no keyboard appears.
        if (!imeShown) return null
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_ENTER_ACTION
        return if (textHandle != 0L) {
            outAttrs.inputType = InputType.TYPE_CLASS_TEXT or
                InputType.TYPE_TEXT_FLAG_AUTO_CORRECT or InputType.TYPE_TEXT_FLAG_MULTI_LINE
            HostTextConnection(this, textHandle)
        } else {
            outAttrs.inputType = InputType.TYPE_NULL
            BaseInputConnection(this, false)
        }
    }

    fun setImeVisible(show: Boolean) {
        val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as? InputMethodManager
            ?: return
        imeShown = show
        if (show) {
            requestFocus()
            // The view may already be focused from a null-connection state — restart so the
            // framework re-queries onCreateInputConnection with the gate now open.
            imm.restartInput(this)
            imm.showSoftInput(this, 0)
        } else {
            imm.hideSoftInputFromWindow(windowToken, 0)
            imm.restartInput(this) // gate closed — drop the editable connection
        }
    }

    /** The stream's own key handling, run before the device IME sees a hardware key. */
    var preIme: ((KeyEvent) -> Boolean)? = null

    /**
     * Hardware keys reach the IME before any view, and a Korean IME answers 한/영 and 한자 itself
     * — so while the keyboard is not summoned the stream claims them first. Summoned, the IME is
     * in the loop on purpose (it composes for the text path) and keeps its first look.
     *
     * BACK while the summoned keyboard is up: the IME consumes it pre-IME to dismiss itself, so
     * [setImeVisible] never hears about it — sync the gate here or a stale `imeShown` leaves the
     * editable connection live and physical typing re-pops the keyboard.
     */
    override fun onKeyPreIme(keyCode: Int, event: KeyEvent): Boolean {
        if (!imeShown && preIme?.invoke(event) == true) return true
        if (keyCode == KeyEvent.KEYCODE_BACK && imeShown && event.action == KeyEvent.ACTION_UP) {
            imeShown = false
            (context.getSystemService(Context.INPUT_METHOD_SERVICE) as? InputMethodManager)
                ?.restartInput(this)
        }
        return super.onKeyPreIme(keyCode, event)
    }
}

/**
 * IME → host text bridge (the `HOST_CAP_TEXT_INPUT` path): a real **editable** connection, so
 * the IME runs its full machinery (autocorrect, gesture typing, non-Latin composition), mirrored
 * to the host as it happens. The one piece of host-side state tracked is *what the host currently
 * shows of the active composition* ([sentComposition]): composing updates send a common-prefix
 * diff (backspaces + the new suffix) so corrections materialize live on the host; a commit
 * settles it. [setComposingRegion] adopts already-committed text as the active composition
 * (autocorrect-revert / backspace-into-word flows), so the next update diffs against it instead
 * of retyping. Newlines become Enter taps; [deleteSurroundingText] becomes Backspace/Delete taps.
 *
 * Known approximation: diff lengths are counted in Unicode scalars, assuming one host Backspace
 * deletes one scalar — true for the composition text IMEs actually produce (emoji and other
 * multi-unit graphemes commit directly rather than composing).
 */
private class HostTextConnection(
    view: KeyCaptureView,
    private val handle: Long,
) : BaseInputConnection(view, true) {
    /** What the host currently shows of the active composition ("" = none). */
    private var sentComposition = ""

    override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
        retype(text.toString())
        sentComposition = ""
        val ok = super.commitText(text, newCursorPosition)
        trimEditable()
        return ok
    }

    override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
        retype(text.toString())
        return super.setComposingText(text, newCursorPosition)
    }

    override fun finishComposingText(): Boolean {
        // The composition text stands as committed — the host already shows it verbatim.
        sentComposition = ""
        return super.finishComposingText()
    }

    override fun setComposingRegion(start: Int, end: Int): Boolean {
        val e = editable
        if (e != null) {
            val a = start.coerceIn(0, e.length)
            val b = end.coerceIn(0, e.length)
            sentComposition = e.subSequence(minOf(a, b), maxOf(a, b)).toString()
        }
        return super.setComposingRegion(start, end)
    }

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        repeat(beforeLength.coerceIn(0, MAX_TAPS)) { tapVk(VK_BACK) }
        repeat(afterLength.coerceIn(0, MAX_TAPS)) { tapVk(VK_DELETE) }
        return super.deleteSurroundingText(beforeLength, afterLength)
    }

    override fun performEditorAction(actionCode: Int): Boolean {
        tapVk(VK_RETURN)
        return true
    }

    /** Replace the host's view of the composition with [text] via a common-prefix diff. */
    private fun retype(text: String) {
        var common = sentComposition.commonPrefixWith(text)
        // Never split a surrogate pair mid-diff — back off to the pair boundary.
        if (common.isNotEmpty() && common.last().isHighSurrogate()) {
            common = common.dropLast(1)
        }
        val stale = sentComposition.substring(common.length)
        repeat(stale.codePointCount(0, stale.length).coerceAtMost(MAX_TAPS)) { tapVk(VK_BACK) }
        sendText(text.substring(common.length))
        sentComposition = text
    }

    /** Forward literal text, turning newlines into Enter taps (control chars never ride text). */
    private fun sendText(s: String) {
        var chunk = StringBuilder()
        for (ch in s) {
            if (ch == '\n') {
                if (chunk.isNotEmpty()) {
                    NativeBridge.nativeSendText(handle, chunk.toString())
                    chunk = StringBuilder()
                }
                tapVk(VK_RETURN)
            } else {
                chunk.append(ch)
            }
        }
        if (chunk.isNotEmpty()) NativeBridge.nativeSendText(handle, chunk.toString())
    }

    private fun tapVk(vk: Int) {
        NativeBridge.nativeSendKey(handle, vk, true, 0)
        NativeBridge.nativeSendKey(handle, vk, false, 0)
    }

    /** Bound the mirror buffer: once nothing is composing, old text serves no purpose. */
    private fun trimEditable() {
        val e = editable ?: return
        if (getComposingSpanStart(e) == -1 && e.length > 4000) e.clear()
    }

    private companion object {
        const val VK_BACK = 0x08
        const val VK_RETURN = 0x0D
        const val VK_DELETE = 0x2E
        const val MAX_TAPS = 256
    }
}

/** The wire pads the controller-mouse toggle acts on: the ring's opener, else every open pad. */
private fun padMouseTarget(ring: RingState, router: GamepadRouter?): Int =
    ring.opener?.let { 1 shl it } ?: (router?.padMask() ?: 0)
