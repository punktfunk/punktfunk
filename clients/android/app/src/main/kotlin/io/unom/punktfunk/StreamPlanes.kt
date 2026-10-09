package io.unom.punktfunk

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.media.audiofx.AcousticEchoCanceler
import android.media.audiofx.AudioEffect
import android.media.audiofx.NoiseSuppressor
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import androidx.core.content.ContextCompat
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.SessionAccess
import io.unom.punktfunk.kit.VideoDecoders
import io.unom.punktfunk.models.ActiveSession
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import kotlin.math.roundToInt

/**
 * The session's media planes on the video surface: video, audio and the mic start in
 * [surfaceCreated] and stop in [surfaceDestroyed], so a recreated surface brings them back.
 * [dispose] stops them for good on the way out; the caller closes the session after it. Every
 * method runs on the main thread; only the mic open leaves it, on [micStarter].
 */
internal class StreamPlanes(
    private val context: Context,
    session: ActiveSession,
    private val ui: StreamUi,
    private val isTv: Boolean,
    private val isChromeOs: Boolean,
    private val streamHz: Int,
    private val streamSize: Pair<Int, Int>?,
) {
    private val handle = session.handle
    private val settings = session.settings
    private val lowLatencyMode = settings.lowLatencyMode

    // A held session keeps its audio running and drains video while the app is away.
    private val keepAlive = keepAliveSpanMs(settings, isTv) != null

    // Mic only if the user enabled it AND granted RECORD_AUDIO (else the AAudio input fails).
    private val micWanted = settings.micEnabled && ContextCompat.checkSelfPermission(
        context,
        Manifest.permission.RECORD_AUDIO,
    ) == PackageManager.PERMISSION_GRANTED

    // The Java AEC/NS pair backstopping the native VoiceCommunication preset, on the audio session
    // `nativeStartMic` returns. Every mic stop releases it, so a surface recreate re-attaches to
    // the fresh stream instead of leaking effect engines. Main thread only.
    private val micEffects = mutableListOf<AudioEffect>()

    // Set by [dispose], which stops every plane before the caller queues the close. The
    // surfaceDestroyed that follows must not restart the keep-alive drain, as the handle stays
    // live until that close runs. A closed handle makes every native call a no-op; this flag is
    // only about that order.
    private val closed = AtomicBoolean(false)

    // The mic opens off the UI thread (AAudio input opens can take hundreds of ms), one start at a
    // time. Every stop bumps `micGen`, so a start that lost its surface meanwhile undoes itself.
    private val micStarter =
        Executors.newSingleThreadExecutor { r -> Thread(r, "pf-mic-start").apply { isDaemon = true } }
    private val micGen = AtomicInteger(0)

    /** Starts video on [holder]'s surface, then audio, then the mic when it is wanted and granted. */
    fun surfaceCreated(holder: SurfaceHolder, view: SurfaceView) {
        // Ends the keep-alive's drain, if one runs: the decode thread takes the frame queue back.
        NativeBridge.nativeVideoDrain(handle, false)
        // Low-latency mode ranks MediaCodecList decoders for the MIME and hands the pick to Rust,
        // which creates it by name with the per-SoC vendor keys. Off: the platform's default.
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
            settings.presentPriorityWire(),
            settings.smoothBuffer,
            // The panel's refresh from the mode TABLE: display.refreshRate reports a per-uid
            // override, not the panel. Fallback: the live rate.
            view.display?.streamPanelFps(streamHz, streamSize)?.takeIf { it > 0 }
                ?: (view.display?.refreshRate ?: 0f).roundToInt(),
            // The view's on-screen size, the space the ASurfaceControl layer composites in. 0
            // before layout, and native falls back to the window buffer size.
            view.width,
            view.height,
        )
        NativeBridge.nativeStartAudio(handle, lowLatencyMode, isTv)
        // The MIC grant is read live: a surface recreate re-runs this. Without it no capture opens;
        // the host never attached its mic service, and the indicator would announce a dead mic.
        if (micWanted && ui.accessGrants and SessionAccess.MIC != 0) {
            val gen = micGen.incrementAndGet()
            val echo = settings.echoCancel
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
                    if (echo) attachMicEffects(sessionId)
                    // Whether a capture opened, not the setting, puts the mute control on
                    // screen. A restart comes back muted if the player muted: the flag lives
                    // on the session handle.
                    ui.micRunning = NativeBridge.nativeMicActive(handle)
                }
            }
        }
    }

    /** Re-reports the view's size and the frame-rate vote; neither change recreates the surface. */
    fun surfaceChanged(holder: SurfaceHolder, view: SurfaceView) {
        // The view grows a frame or two after the screen appears (bars hidden, cutout drawing). A
        // layer left on the start-up rect paints the picture small, top-left. The view's own
        // size, not the buffer geometry: the layer composites in the view's space.
        NativeBridge.nativeVideoSurfaceSize(handle, view.width, view.height)
        // Some OEM builds reset the frame-rate vote on a geometry change, dropping the 120 Hz pin.
        // The native hint's policy: ALWAYS only on the TV low-latency path, so a phone's re-hint
        // never forces a mode flicker.
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

    /** Backgrounding, or on the way out: stops what renders to the surface, unless [dispose] did. */
    fun surfaceDestroyed() {
        if (!closed.get()) {
            micGen.incrementAndGet()
            stopMic()
            // The keep-alive keeps audio running. Video stops either way, but a held session must
            // keep popping access units, or the queue stands and the client asks the host for a
            // keyframe every two seconds until the player is back.
            if (!keepAlive) NativeBridge.nativeStopAudio(handle)
            NativeBridge.nativeStopVideo(handle)
            if (keepAlive) NativeBridge.nativeVideoDrain(handle, true)
        }
    }

    /**
     * Stops the capture and drops its effects. The mute stays on the handle, so the next start
     * comes back with the player's choice.
     */
    fun stopMic() {
        releaseMicEffects()
        NativeBridge.nativeStopMic(handle)
        ui.micRunning = false
    }

    /** Leaving the stream: stops the mic, audio and decode threads for good. The caller closes after. */
    fun dispose() {
        closed.set(true) // a later surfaceDestroyed stops nothing more and starts no drain
        micGen.incrementAndGet()
        micStarter.shutdown()
        releaseMicEffects()
        NativeBridge.nativeStopMic(handle)
        NativeBridge.nativeStopAudio(handle)
        NativeBridge.nativeStopVideo(handle)
        NativeBridge.nativeVideoDrain(handle, false)
    }

    /**
     * Attach the Java echo-canceller + noise-suppressor pair to the mic stream's audio session — the
     * backstop for HALs whose VoiceCommunication capture path doesn't cancel on its own (the native
     * side already opened the stream under that preset). [sessionId] `<= 0` means native allocated no
     * session (echo cancellation off, or the preset fell back to the plain open), so there is nothing
     * to hang an effect on. `create()` returning null (unsupported / claimed) is quietly nothing —
     * the HAL preset still does its part. Needs no extra permission: the effect APIs attach to our
     * own recording session.
     */
    private fun attachMicEffects(sessionId: Int) {
        if (sessionId <= 0) return
        if (AcousticEchoCanceler.isAvailable()) {
            AcousticEchoCanceler.create(sessionId)?.let { it.setEnabled(true); micEffects.add(it) }
        }
        if (NoiseSuppressor.isAvailable()) {
            NoiseSuppressor.create(sessionId)?.let { it.setEnabled(true); micEffects.add(it) }
        }
    }

    /** Release every attached mic effect engine. Idempotent: the list is cleared. */
    private fun releaseMicEffects() {
        micEffects.forEach { runCatching { it.release() } }
        micEffects.clear()
    }
}
