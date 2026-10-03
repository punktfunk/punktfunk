package io.unom.punktfunk

import android.os.SystemClock
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.spring
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.wrapContentHeight
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.blur
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.boundsInWindow
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import coil.ImageLoader
import coil.compose.AsyncImage
import coil.request.ImageRequest
import io.unom.punktfunk.kit.library.Download
import io.unom.punktfunk.kit.library.GameEnd
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.LibraryClient
import io.unom.punktfunk.kit.security.IdentityHolder
import io.unom.punktfunk.models.LaunchHold
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * How long the hold waits on a title the host still calls `launching`, or `running` without its
 * window (a cold Steam boot with shader work runs to minutes), and on one the host never lists
 * (the launch did not resolve).
 */
private const val LAUNCH_HOLD_MAX_S = 120.0
private const val LAUNCH_NO_LEASE_S = 15.0

/**
 * Why the hold is giving up, in one sentence, or null while it should keep waiting.
 *
 * [state] is the host's own `games[]` word for this title, null when the host lists nothing for it
 * at all — which is what a refused launch looks like from here. Every sentence this returns ends
 * the hold with a message and its three actions; `running`, `untracked` and `grace` return null
 * and the caller shows the stream, because those are launches that worked.
 */
internal fun launchGaveUp(title: String, state: String?, elapsed: Double): String? = when {
    state == null && elapsed >= LAUNCH_NO_LEASE_S ->
        "The host didn't start $title \u2014 nothing is running for it."
    state == "launching" && elapsed >= LAUNCH_HOLD_MAX_S ->
        "$title is still starting after 2 minutes."
    state == "exited" -> "$title closed right after starting."
    else -> null
}

/**
 * Keep holding: the host lists nothing yet, still calls it `launching`, or has it `running` with a
 * window still to come. Past [LAUNCH_HOLD_MAX_S] a running game is shown as it is.
 */
internal fun launchStillHeld(state: String?, awaitingWindow: Boolean, elapsed: Double): Boolean =
    state == null || state == "launching" ||
        (state == "running" && awaitingWindow && elapsed < LAUNCH_HOLD_MAX_S)

/** The cover's flight: `response ≈ 0.75 s`, loose enough that the turn reads on the way. */
private const val FLIGHT_STIFFNESS = 70f
private const val FLIGHT_DAMPING = 0.72f

/**
 * Where each shelf tile last drew, in window coordinates, by library id.
 *
 * The launch hold's cover flies out of the tile the player picked, and by then that tile is gone —
 * the shelf is replaced by the stream. So the rect is recorded as the shelf lays out and read once,
 * at the tap. A tile that scrolled away keeps its last rect; the hold checks it is still on screen
 * and otherwise just scales the cover up in place.
 */
object TileFrames {
    private val frames = mutableMapOf<String, Rect>()

    fun record(id: String, rect: Rect) {
        frames[id] = rect
    }

    fun rect(id: String): Rect? = frames[id]?.takeIf { it.width > 1f && it.height > 1f }
}

/**
 * The launch hold: the picked title's cover leaves its shelf tile and holds the screen until the
 * game is actually up.
 *
 * Opaque, so the launcher and desktop behind it are never seen — that is the whole job. Polls the
 * host's `/status` once a second (the Resume badge's lane) and calls [onShow] once the title is up;
 * a tap does the same. When the launch did not produce a game it says so and offers the three moves
 * ([launchGaveUp]), rather than sliding away and leaving the player on a desktop.
 */
@Composable
fun LaunchHoldOverlay(hold: LaunchHold, onRetry: () -> Unit, onShow: () -> Unit) {
    val context = LocalContext.current
    val density = LocalDensity.current
    var loader by remember(hold) { mutableStateOf<ImageLoader?>(null) }
    val flight = remember(hold) { Animatable(0f) }
    var rootInWindow by remember(hold) { mutableStateOf(Rect.Zero) }
    // Non-null replaces the spinner with the message and its actions; the hold stops polling then.
    var gaveUp by remember(hold) { mutableStateOf<String?>(null) }
    var ending by remember(hold) { mutableStateOf<String?>(null) }
    var windowWait by remember(hold) { mutableStateOf(false) }
    var download by remember(hold) { mutableStateOf<Download?>(null) }
    val scope = rememberCoroutineScope()

    LaunchedEffect(hold) {
        flight.animateTo(
            1f,
            spring(dampingRatio = FLIGHT_DAMPING, stiffness = FLIGHT_STIFFNESS),
        )
    }
    LaunchedEffect(hold) {
        val (id, art) = withContext(Dispatchers.IO) {
            IdentityHolder.shared(context).await()?.let { me ->
                runCatching { me to posterLoader(context, me, hold.address, hold.fpHex) }.getOrNull()
            }
        } ?: run {
            onShow()
            return@LaunchedEffect
        }
        loader = art
        var began = SystemClock.elapsedRealtime()
        while (isActive) {
            val status = withContext(Dispatchers.IO) {
                LibraryClient.fetchStatus(
                    hold.address, hold.mgmtPort, id.certPem, id.privateKeyPem, hold.fpHex,
                )
            }
            val game = status.games.firstOrNull { it.appId == hold.game.id }
            val state = game?.state
            val dl = status.downloads.firstOrNull { it.appId == hold.game.id }
            // The host fetches the title's files before it starts it: no cap while they come, and
            // the clocks start over once they are in.
            if (dl?.live == true) {
                download = dl
                began = SystemClock.elapsedRealtime()
                delay(1_000)
                continue
            }
            download = null
            // A download that stopped is why nothing started: that, not the generic sentence.
            val stopped = dl?.takeIf { state == null || state == "launching" }
                ?.stopped(hold.game.title)
            val elapsed = (SystemClock.elapsedRealtime() - began) / 1000.0
            val said = stopped ?: launchGaveUp(hold.game.title, state, elapsed)
            if (said != null) {
                gaveUp = said
                return@LaunchedEffect
            }
            // Anything else the host names is a launch that worked.
            if (!launchStillHeld(state, game?.awaitingWindow == true, elapsed)) {
                onShow()
                return@LaunchedEffect
            }
            windowWait = state == "running"
            delay(1_000)
        }
    }

    BoxWithConstraints(
        Modifier
            .fillMaxSize()
            .onGloballyPositioned { rootInWindow = it.boundsInWindow() }
            // Only while it is still waiting: a stray tap must not take the message away.
            .clickable(
                enabled = gaveUp == null,
                interactionSource = remember { MutableInteractionSource() },
                indication = null,
                onClick = onShow,
            ),
    ) {
        val w = with(density) { maxWidth.toPx() }
        val h = with(density) { maxHeight.toPx() }
        // Cover and column as one centred pair. The cover wants most of the height, but on a
        // portrait phone that would be wider than the screen — so the column and the margins
        // are taken out of the width first and the card gets what is left.
        val gap = minOf(w * 0.04f, with(density) { 40.dp.toPx() })
        val detailsW = maxOf(w * 0.38f, with(density) { 150.dp.toPx() })
        val maxCoverW = w - gap - detailsW - with(density) { 32.dp.toPx() }
        var coverH = minOf(h * 0.62f, with(density) { 460.dp.toPx() })
        var coverW = coverH * 2f / 3f
        if (coverW > maxCoverW) {
            coverW = maxCoverW.coerceAtLeast(1f)
            coverH = coverW * 1.5f
        }
        val x0 = (w - (coverW + gap + detailsW)) / 2f
        val top = (h - coverH) / 2f
        val settled = Rect(left = x0, top = top, right = x0 + coverW, bottom = top + coverH)
        // Where it flies from: the tile the player tapped, in this overlay's own space. With no
        // usable tile the cover just arrives, a little small, rather than flying in from nowhere.
        val source = hold.sourceRect
            ?.translate(-rootInWindow.left, -rootInWindow.top)
            ?.takeIf { it.overlaps(Rect(-80f, -80f, w + 80f, h + 80f)) }
        val start = source ?: Rect(
            left = settled.left + settled.width * 0.07f,
            top = settled.top + settled.height * 0.07f,
            right = settled.right - settled.width * 0.07f,
            bottom = settled.bottom - settled.height * 0.07f,
        )
        val p = flight.value
        val card = Rect(
            left = start.left + (settled.left - start.left) * p,
            top = start.top + (settled.top - start.top) * p,
            right = start.right + (settled.right - start.right) * p,
            bottom = start.bottom + (settled.bottom - start.bottom) * p,
        )
        // The backdrop closes over the shelf while the cover is still crossing it, so the cover is
        // seen LEAVING its tile rather than appearing on a screen that already replaced it.
        val veil = p.coerceIn(0f, 1f)

        Box(Modifier.fillMaxSize().background(Color.Black.copy(alpha = veil))) {
            loader?.let { l ->
                // The title's own art, thrown out of focus behind it — the game colours the room
                // it is starting in.
                AsyncImage(
                    model = ImageRequest.Builder(context)
                        .data(hold.game.art.posterCandidates.firstOrNull()).build(),
                    imageLoader = l,
                    contentDescription = null,
                    contentScale = ContentScale.Crop,
                    modifier = Modifier
                        .fillMaxSize()
                        .graphicsLayer { alpha = 0.35f * veil; scaleX = 1.25f; scaleY = 1.25f }
                        .blur(60.dp),
                )
                Box(Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.45f * veil)))
            }
        }
        Box(
            Modifier
                .offset { IntOffset(card.left.toInt(), card.top.toInt()) }
                .size(
                    with(density) { card.width.toDp() },
                    with(density) { card.height.toDp() },
                )
                // One full turn on the way over — around the card's own vertical axis, so it
                // reads as a cover turning rather than a picture spinning flat.
                .graphicsLayer {
                    rotationY = 360f * p
                    // Viewer distance in pixels, taken from the card rather than a constant, so
                    // the turn has the same depth at any size — a couple of card widths, which
                    // is the Apple hold's `CoverFlip.distance`.
                    cameraDistance = 2.2f * size.width
                }
                .shadow(24.dp, RoundedCornerShape(14.dp))
                .clip(RoundedCornerShape(14.dp))
                .background(Color(0xFF1E1E26)),
            contentAlignment = Alignment.Center,
        ) {
            loader?.let { HoldPosterArt(hold.game, it) }
        }
        // Centred against the cover rather than hung from its top: most titles carry one fact
        // line and no studio, and a block pinned to the top of a card this tall reads as having
        // fallen off it.
        Box(
            Modifier
                .offset {
                    IntOffset((settled.right + gap).toInt(), settled.top.toInt())
                }
                .width(with(density) { detailsW.toDp() })
                .height(with(density) { coverH.toDp() })
                .graphicsLayer { alpha = veil },
            contentAlignment = Alignment.CenterStart,
        ) {
        Column {
            Text(
                hold.game.title,
                color = Color.White,
                fontSize = 26.sp,
                fontWeight = FontWeight.SemiBold,
                lineHeight = 30.sp,
                maxLines = 3,
                overflow = TextOverflow.Ellipsis,
            )
            val facts = listOfNotNull(
                hold.game.platform,
                hold.game.releaseYear?.toString(),
                hold.game.storeLabel,
            ).joinToString(" · ")
            if (facts.isNotEmpty()) {
                Text(
                    facts,
                    color = Color.White.copy(alpha = 0.62f),
                    fontSize = 14.sp,
                    fontWeight = FontWeight.Medium,
                    modifier = Modifier.padding(top = 10.dp),
                )
            }
            hold.game.developer?.takeIf { it.isNotBlank() }?.let {
                Text(
                    it,
                    color = Color.White.copy(alpha = 0.45f),
                    fontSize = 13.sp,
                    modifier = Modifier.padding(top = 4.dp),
                )
            }
            if (hold.game.genres.isNotEmpty()) {
                Text(
                    hold.game.genres.joinToString(" · "),
                    color = Color.White.copy(alpha = 0.45f),
                    fontSize = 13.sp,
                    modifier = Modifier.padding(top = 4.dp),
                )
            }
            val said = gaveUp
            val dl = download
            if (said == null && dl != null) {
                // The wait has a length now: a bar where the spinner was. No Show stream — there
                // is no stream until the files are in.
                val fraction = dl.fraction
                if (fraction != null) {
                    LinearProgressIndicator(
                        progress = { fraction },
                        color = Color.White,
                        trackColor = Color.White.copy(alpha = 0.15f),
                        modifier = Modifier.padding(top = 24.dp).widthIn(max = 360.dp).fillMaxWidth(),
                    )
                } else {
                    LinearProgressIndicator(
                        color = Color.White,
                        trackColor = Color.White.copy(alpha = 0.15f),
                        modifier = Modifier.padding(top = 24.dp).widthIn(max = 360.dp).fillMaxWidth(),
                    )
                }
                Text(
                    if (dl.state == "downloading") "Downloading · ${dl.line()}" else dl.line(),
                    color = Color.White.copy(alpha = 0.7f),
                    fontSize = 13.sp,
                    modifier = Modifier.padding(top = 10.dp),
                )
                Text(
                    "Leaving won't stop the download.",
                    color = Color.White.copy(alpha = 0.4f),
                    fontSize = 12.sp,
                    modifier = Modifier.padding(top = 4.dp),
                )
            } else if (said == null) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier.padding(top = 22.dp),
                ) {
                    CircularProgressIndicator(
                        modifier = Modifier.size(16.dp),
                        color = Color.White,
                        strokeWidth = 2.dp,
                    )
                    Text(
                        if (windowWait) "Waiting for the game's window\u2026" else "Connecting\u2026",
                        color = Color.White.copy(alpha = 0.5f),
                        fontSize = 13.sp,
                        modifier = Modifier.padding(start = 9.dp),
                    )
                }
                TextButton(onClick = onShow, modifier = Modifier.padding(top = 8.dp)) {
                    Text("Show stream", color = Color.White)
                }
            } else {
                Text(
                    said,
                    color = Color.White.copy(alpha = 0.85f),
                    fontSize = 15.sp,
                    lineHeight = 21.sp,
                    modifier = Modifier.padding(top = 22.dp),
                )
                ending?.let {
                    Text(
                        it,
                        color = Color.White.copy(alpha = 0.5f),
                        fontSize = 13.sp,
                        modifier = Modifier.padding(top = 8.dp),
                    )
                }
                // Stacked, not a row: three labels this long wrap to nothing readable on a phone.
                TextButton(onClick = onRetry, modifier = Modifier.padding(top = 10.dp)) {
                    Text("Retry", color = Color.White)
                }
                TextButton(onClick = onShow) {
                    Text("Show the desktop anyway", color = Color.White)
                }
                TextButton(
                    enabled = ending == null,
                    onClick = {
                        ending = "Ending it\u2026"
                        scope.launch {
                            val id = withContext(Dispatchers.IO) {
                                IdentityHolder.shared(context).await()
                            }
                            val outcome = if (id == null) {
                                GameEnd.Failed("this device has no identity yet")
                            } else {
                                withContext(Dispatchers.IO) {
                                    LibraryClient.endGame(
                                        hold.address, hold.mgmtPort, id.certPem, id.privateKeyPem,
                                        hold.fpHex, hold.game.id,
                                    )
                                }
                            }
                            ending = when (outcome) {
                                GameEnd.Ended -> "Ended it \u2014 press Retry to start it again."
                                GameEnd.NotRunning -> "The host had nothing running for it."
                                else -> outcome.notice(hold.game.title)
                            }
                        }
                    },
                ) {
                    Text("End it on the host", color = Color.White)
                }
            }
        }
        }
    }
}

/** The shelf's poster: each candidate in turn, the title when none loads. */
@Composable
private fun HoldPosterArt(game: GameEntry, loader: ImageLoader) {
    PosterArt(game, loader) {
        Text(
            game.title,
            color = Color.White.copy(alpha = 0.7f),
            textAlign = TextAlign.Center,
            modifier = Modifier.fillMaxSize().wrapContentHeight().padding(12.dp),
        )
    }
}
