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
) {
    var invertScroll by mutableStateOf(initialInvertScroll)
        private set

    fun setScrollInverted(inverted: Boolean) {
        if (accessGrants and SessionAccess.POINTER == 0 || inverted == invertScroll) return
        if (applyInvertScroll(inverted)) invertScroll = inverted
    }

    /** The session's grants (per-client access), the courtesy mirror of what the host enforces. */
    var accessGrants by mutableIntStateOf(initialAccess?.getOrNull(0) ?: SessionAccess.ALL)

    /** Seconds until this session's access expires (0 = permanent), as last reported natively. */
    var accessRemaining by mutableIntStateOf(initialAccess?.getOrNull(1) ?: 0)

    /**
     * In-stream mic mute. Per SESSION and never persisted: a new stream always starts unmuted. The
     * authoritative flag lives on the native handle, which is why a mute survives the mic
     * stop/start a surface recreate performs — this is the UI's mirror of it.
     */
    var micMuted by mutableStateOf(false)
        private set

    /** Whether a capture is actually RUNNING, not merely wanted — from what nativeMicActive reports. */
    var micRunning by mutableStateOf(false)

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
     * Whether this session has a controller — seeded from the router the moment it is built and
     * latched true by a pad that arrives later; it never goes back to false.
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
     * The one place mute is toggled: Compose state, the native flag and the hint that says so,
     * always together. The chord and the dial both come through here.
     */
    fun mute(muted: Boolean) {
        micMuted = muted
        NativeBridge.nativeSetMicMuted(handle, muted)
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

/** The Access chip's time left in the app's language: "1 h 58 m", "12 m", "45 s". */
internal fun Context.accessRemaining(secs: Int): String =
    SessionAccess.remaining(secs).let { getString(it.message, *it.args.toTypedArray()) }
