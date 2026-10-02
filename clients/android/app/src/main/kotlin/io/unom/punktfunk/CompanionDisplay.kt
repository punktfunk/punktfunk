package io.unom.punktfunk

import android.app.Presentation
import android.content.Context
import android.hardware.display.DisplayManager
import android.os.Build
import android.util.Log
import android.view.Display
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.Window
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.ComposeView
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import io.unom.punktfunk.kit.NativeBridge
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.setViewTreeLifecycleOwner
import androidx.lifecycle.setViewTreeViewModelStoreOwner
import androidx.savedstate.setViewTreeSavedStateRegistryOwner
import java.util.Locale
import kotlin.math.hypot
import kotlin.math.roundToInt

/*
 * Shape (b) of design/android-dual-screen.md: the second screen is its own `Display`, and the
 * companion panel goes there as a `Presentation` while the stream keeps the activity's display.
 */

/**
 * A screen you hold, not one you watch. A TV on USB-C and a cast target are displays too, and
 * they must keep mirroring the stream rather than show its controls.
 */
private const val COMPANION_MAX_INCHES = 7f

/** The panel's physical diagonal, or infinity when the display reports no density to measure by. */
private fun Display.diagonalInches(context: Context): Float {
    val m = context.createDisplayContext(this).resources.displayMetrics
    if (m.xdpi <= 0f || m.ydpi <= 0f) return Float.POSITIVE_INFINITY
    return hypot(m.widthPixels / m.xdpi, m.heightPixels / m.ydpi)
}

/** A public display, other than the default and [context]'s own, small enough to be held. */
internal fun companionDisplay(context: Context, dm: DisplayManager): Display? {
    val own = ContextCompat.getDisplayOrDefault(context).displayId
    return dm.displays.firstOrNull { d ->
        d.displayId != Display.DEFAULT_DISPLAY && d.displayId != own &&
            d.flags and Display.FLAG_PRIVATE == 0 && d.diagonalInches(context) <= COMPANION_MAX_INCHES
    }
}

/** The screen the picture goes to: the second screen the player put it on, else [own]. */
internal fun pictureDisplay(context: Context, own: Display): Display {
    val dm = context.getSystemService(DisplayManager::class.java) ?: return own
    return companionDisplay(context, dm)
        ?.takeIf { CompanionMemory.layout(context, it.name) == ScreenLayout.SWAPPED } ?: own
}

/** True while the pair's kept layout spans the picture across a second display and this one. */
internal fun pictureSpanned(context: Context): Boolean {
    val dm = context.getSystemService(DisplayManager::class.java) ?: return false
    return companionDisplay(context, dm)?.let { CompanionMemory.layout(context, it.name) == ScreenLayout.SPANNED } == true
}

/**
 * The second picture window (design §4): a `SurfaceView` the presenter's second layer composites
 * into — the picture on a second display, or its lower half there or below a hinge. Attached
 * and detached as the surface comes and goes; the decoder never restarts for it.
 */
@Composable
internal fun PictureSurface(handle: Long, modifier: Modifier = Modifier) {
    AndroidView(
        modifier = modifier,
        factory = { ctx ->
            SurfaceView(ctx).apply {
                holder.addCallback(object : SurfaceHolder.Callback {
                    override fun surfaceCreated(holder: SurfaceHolder) {
                        NativeBridge.nativePictureWindow(handle, holder.surface, this@apply.width, this@apply.height)
                    }

                    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
                        NativeBridge.nativePictureSurfaceSize(handle, this@apply.width, this@apply.height)
                    }

                    override fun surfaceDestroyed(holder: SurfaceHolder) {
                        NativeBridge.nativePictureWindow(handle, null, 0, 0)
                    }
                })
            }
        },
    )
}

/** Every display as one `pf.display` line: what a dual-screen device reports, for its bundle. */
internal fun describeDisplays(context: Context, dm: DisplayManager): String {
    val presentation = dm.getDisplays(DisplayManager.DISPLAY_CATEGORY_PRESENTATION).map { it.displayId }
    return dm.displays.joinToString(" | ") { d ->
        val m = context.createDisplayContext(d).resources.displayMetrics
        "id=${d.displayId} \"${d.name}\" ${m.widthPixels}x${m.heightPixels} dpi=${m.densityDpi} " +
            "xdpi=${m.xdpi.roundToInt()} in=${String.format(Locale.ROOT, "%.1f", d.diagonalInches(context))} " +
            "flags=0x${Integer.toHexString(d.flags)} presentation=${d.displayId in presentation}"
    }
}

/** The companion display while one is attached; follows displays arriving and leaving. */
@Composable
internal fun rememberCompanionDisplay(): Display? {
    val context = LocalContext.current
    val dm = remember(context) { context.getSystemService(DisplayManager::class.java) } ?: return null
    var display by remember(dm) { mutableStateOf(companionDisplay(context, dm)) }
    DisposableEffect(dm) {
        val listener = object : DisplayManager.DisplayListener {
            override fun onDisplayAdded(displayId: Int) { display = companionDisplay(context, dm) }
            override fun onDisplayRemoved(displayId: Int) { display = companionDisplay(context, dm) }
            override fun onDisplayChanged(displayId: Int) {}
        }
        dm.registerDisplayListener(listener, null)
        onDispose { dm.unregisterDisplayListener(listener) }
    }
    return display
}

/**
 * [content] on [display], as a `Presentation` shown while the activity is started. Its window is
 * not focusable: a tap on a focusable window moves key focus to that display, and the handheld's
 * own buttons would stop reaching the stream. A [pictureHz] means the window holds the picture:
 * it stays lit, pins the panel to the stream's refresh and takes input unbuffered, as
 * [StreamWindow] does for the activity's window.
 */
@Composable
internal fun CompanionOnDisplay(display: Display, pictureHz: Int? = null, content: @Composable () -> Unit) {
    val activity = LocalContext.current as? ComponentActivity ?: return
    val latest by rememberUpdatedState(content)
    // A Presentation cancels itself when its display's metrics change; the next one takes the new ones.
    var generation by remember { mutableIntStateOf(0) }
    DisposableEffect(activity, display.displayId, generation, pictureHz) {
        val p = Presentation(activity, display)
        p.window?.addFlags(WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE)
        // Its own window, so the app theme the activity's composition sits under is set again here.
        val view = ComposeView(p.context).apply {
            setViewTreeLifecycleOwner(activity)
            setViewTreeViewModelStoreOwner(activity)
            setViewTreeSavedStateRegistryOwner(activity)
            setContent { PunktfunkTheme { latest() } }
        }
        if (pictureHz != null) p.window?.let { holdPicture(it, view, display, pictureHz) }
        p.setContentView(view)
        var disposed = false
        p.setOnDismissListener { if (!disposed) generation++ }
        // Added while started, the observer sees ON_START at once: that is the first show.
        val observer = LifecycleEventObserver { _, event ->
            when (event) {
                Lifecycle.Event.ON_START -> runCatching { p.show() }
                    .onFailure { Log.w("pf.display", "companion display ${display.displayId} refused the panel", it) }
                Lifecycle.Event.ON_STOP -> p.hide()
                else -> {}
            }
        }
        activity.lifecycle.addObserver(observer)
        onDispose {
            disposed = true
            activity.lifecycle.removeObserver(observer)
            p.dismiss()
        }
    }
}

/**
 * The picture's window: lit, pinned to a [hz] stream's mode, input off the vsync batch. Dies with
 * it. `0` pins nothing: the lower half of a spanned picture keeps the panel's own rate.
 */
private fun holdPicture(window: Window, view: View, display: Display, hz: Int) {
    window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    if (hz > 0) display.streamModeFor(hz)?.let { m ->
        window.attributes = window.attributes.apply { preferredDisplayModeId = m.modeId }
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) view.requestUnbufferedDispatch(STREAM_UNBUFFERED_SOURCES)
    if (Build.VERSION.SDK_INT >= 35 && hz > 0) view.requestedFrameRate = hz.toFloat()
}
