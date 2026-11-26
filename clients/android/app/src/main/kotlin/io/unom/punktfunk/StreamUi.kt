package io.unom.punktfunk

import android.content.Context
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.SessionAccess

/**
 * The stream screen's Compose state that the session's peripherals write from their callbacks —
 * held in one object so [StreamPeripherals] can be a plain class over it instead of a closure over
 * the composable's scope. Per session: created with the handle.
 */
internal class StreamUi(
    private val handle: Long,
    initialAccess: IntArray?,
    initialVerbosity: StatsVerbosity,
    initialInvertScroll: Boolean = false,
    private val applyInvertScroll: (Boolean) -> Boolean = { NativeBridge.nativeSetInvertScroll(handle, it) },
    /** The stream passes [HostTouchpad.read]; without it no source sends frames. */
    private val readTouchpad: () -> HostTouchpad = { HostTouchpad.NONE },
    /** The mute the player left last time ([MicMuteMemory]); the session starts from it. */
    initialMicMuted: Boolean = false,
    private val applyMicMuted: (Boolean) -> Unit = { NativeBridge.nativeSetMicMuted(handle, it) },
    /** Saves each mute change for the next session. */
    private val keepMicMuted: (Boolean) -> Unit = {},
) {
    var invertScroll by mutableStateOf(initialInvertScroll)
        private set

    /** The host's touchpad as core answers it. Core reads the invert flag into that answer, so it
     *  is read only after the flag reaches core: at [seed] and after each change of it. */
    var touchpad by mutableStateOf(HostTouchpad.NONE)
        private set

    /**
     * Session start: the invert flag reaches core, then the touchpad is read. A saved mute
     * reaches the native flag here, before [StreamPlanes] opens the capture, so no audio leaves
     * ahead of it.
     */
    fun seed() {
        applyInvertScroll(invertScroll)
        touchpad = readTouchpad()
        if (micMuted) applyMicMuted(true)
    }

    fun setScrollInverted(inverted: Boolean) {
        if (accessGrants and SessionAccess.POINTER == 0 || inverted == invertScroll) return
        if (applyInvertScroll(inverted)) {
            invertScroll = inverted
            touchpad = readTouchpad()
        }
    }

    /** The session's grants (per-client access), the courtesy mirror of what the host enforces. */
    var accessGrants by mutableIntStateOf(initialAccess?.getOrNull(0) ?: SessionAccess.ALL)

    /** Seconds until this session's access expires (0 = permanent), as last reported natively. */
    var accessRemaining by mutableIntStateOf(initialAccess?.getOrNull(1) ?: 0)

    /**
     * In-stream mic mute. [mute] saves each change as [MicMuteMemory] and the next session starts
     * from it. The authoritative flag lives on the native handle, which is why a mute survives the
     * mic stop/start a surface recreate performs — this is the UI's mirror of it.
     */
    var micMuted by mutableStateOf(initialMicMuted)
        private set

    /** Whether a capture is actually RUNNING, not merely wanted — from what nativeMicActive reports. */
    var micRunning by mutableStateOf(false)
        private set

    /** Whether this session has told the player its saved mute; a surface recreate reopens the mic. */
    private var toldSavedMute = false

    /**
     * A capture start's outcome. The session's first running capture under a saved mute shows the
     * mute hint once, as a toggle would.
     */
    fun micStarted(running: Boolean) {
        micRunning = running
        if (running && micMuted && !toldSavedMute) {
            toldSavedMute = true
            micHint = R.string.notice_mic_muted
        }
    }

    /** The capture stopped; the mute stays on the native handle. */
    fun micStopped() {
        micRunning = false
    }

    /** Transient confirmation of a mic toggle, whichever control made it: a string resource, or null for nothing. */
    var micHint by mutableStateOf<Int?>(null)

    /** A fired shortcut's line: why its action is dimmed, or "press again". Shown briefly. */
    var shortcutHint by mutableStateOf<String?>(null)

    /** A captured pad has a gyro this session's virtual controller cannot carry. Shown briefly. */
    var motionHint by mutableStateOf(false)

    /**
     * The stream holds the screen awake ([NativeBridge.nativeHoldsScreenAwake]). The activity's
     * window and a second screen's picture window both follow it.
     */
    var screenHeld by mutableStateOf(true)

    /**
     * Whether this session has a controller — seeded from the connected controllers when
     * [StreamPeripherals] starts and latched true by a pad that arrives later; it never goes back
     * to false.
     */
    var padPresent by mutableStateOf(false)

    /** The gamepad exit chord is held and counting down — drives the "hold to quit" hint. */
    var exitArming by mutableStateOf(false)

    /** The TV remote is acting as a pointer (hold SELECT toggles) — drives the mode hint. */
    var remotePointerOn by mutableStateOf(false)

    /** The host's pointer for the mouse to wear ([HostCursor]); `null` hides the mouse pointer. */
    var hostPointer by mutableStateOf<HostShape?>(null)

    /** The stats HUD tier, cycled live by the three-finger tap or the Select + X chord. */
    var statsVerbosity by mutableStateOf(initialVerbosity)

    /**
     * Why the stream is silent: `1` this device, `2` the host's per-session mute, `3` both. The
     * local bit is written here the moment the player flips it; the host's arrives on the poll.
     */
    var audioMute by mutableIntStateOf(0)

    /**
     * The one place mute is toggled: Compose state, the native flag, the saved mute and the hint
     * that says so, always together. The chord and the dial both come through here.
     */
    fun mute(muted: Boolean) {
        micMuted = muted
        applyMicMuted(muted)
        keepMicMuted(muted)
        micHint = if (muted) R.string.notice_mic_muted else R.string.notice_mic_live
    }

    /** Same, for this device's speakers. Never reaches the host — see [NativeBridge.nativeSetStreamMuted]. */
    fun muteStream(muted: Boolean) {
        audioMute = if (muted) audioMute or AUDIO_MUTE_LOCAL else audioMute and AUDIO_MUTE_LOCAL.inv()
        NativeBridge.nativeSetStreamMuted(handle, muted)
    }

    /** This device's own toggle, apart from the host's. */
    val streamMuted: Boolean get() = audioMute and AUDIO_MUTE_LOCAL != 0

    /**
     * The string resource for the overlay's sentence about the mute mask, `null` while the stream
     * is audible. Twin of the Rust `audio_mute_label`: unmuting locally must not read as sound
     * being back.
     */
    val audioMuteLabel: Int?
        get() = when {
            audioMute and AUDIO_MUTE_HOST != 0 && audioMute and AUDIO_MUTE_LOCAL != 0 ->
                R.string.notice_audio_muted_host_and_device
            audioMute and AUDIO_MUTE_HOST != 0 -> R.string.notice_audio_muted_host
            audioMute and AUDIO_MUTE_LOCAL != 0 -> R.string.notice_audio_muted_device
            else -> null
        }

    companion object {
        /** `punktfunk_core::client::AUDIO_MUTE_LOCAL` / `AUDIO_MUTE_HOST`, as the JNI reports them. */
        const val AUDIO_MUTE_LOCAL = 1
        const val AUDIO_MUTE_HOST = 2
    }
}

/** The Access chip's time left in the app's language: "1 h 58 min", "12 min", "45 s". */
internal fun Context.accessRemaining(secs: Int): String =
    SessionAccess.remaining(secs).let { getString(it.message, *it.args.toTypedArray()) }

/**
 * The in-stream mic mute as the player last left it, on this device. Not a setting: [StreamUi.mute]
 * writes it and a new stream starts from it. Stream microphone off still opens no capture.
 */
internal object MicMuteMemory {
    private const val PREFS = "punktfunk_mic_mute"
    private const val MUTED = "muted"

    private fun prefs(context: Context) =
        context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    fun muted(context: Context): Boolean = prefs(context).getBoolean(MUTED, false)

    fun keep(context: Context, muted: Boolean) {
        prefs(context).edit().putBoolean(MUTED, muted).apply()
    }
}
