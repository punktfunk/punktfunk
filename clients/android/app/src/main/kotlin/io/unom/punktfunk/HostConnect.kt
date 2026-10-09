package io.unom.punktfunk

import android.content.Context
import android.util.Log
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.ConnectRequest
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.VideoDecoders
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.models.ActiveSession
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.isActive
import kotlinx.coroutines.withContext
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/** Handshake budget for a normal / library-launch connect (not the long request-access park). */
const val CONNECT_TIMEOUT_MS = 10_000

/**
 * Handshake budget for the no-PIN "request access" connect. Must exceed the host's approval-park
 * window (~180 s) so a slow operator approval still lands on this same parked connection rather than
 * timing the client out first. Mirrors the Linux client's 185 s.
 */
const val REQUEST_ACCESS_TIMEOUT_MS = 185_000

/** What every shell does once [connectToHost] hands back a session handle. */
object SessionFactory {
    /**
     * The session a dial opened, for the stream screen. [settings] is what the dial used. The
     * clipboard decision is [record]'s: a host never saved gets none until the user enables it.
     * The Welcome's management port is saved on [record]: it is the one source that needs no mDNS
     * advert, so a host that moved off 47990 stays browsable over a VPN or when added by address.
     * `0` means not advertised and is ignored.
     */
    fun afterDial(
        handle: Long,
        record: KnownHost?,
        settings: Settings,
        preset: StreamPreset?,
        store: KnownHostStore,
        mgmtPort: Int = NativeBridge.nativeHostMgmtPort(handle),
    ): ActiveSession {
        if (record != null) store.learnMgmtPort(record, mgmtPort)
        return ActiveSession(
            handle,
            settings,
            clipboardSync = record?.clipboardSync ?: false,
            presetName = preset?.name,
            hostId = record?.id,
        )
    }

    /** Save the identity the host on [handle] presented; `null` when it presented none. */
    fun pinPresented(
        handle: Long,
        host: String,
        port: Int,
        name: String,
        paired: Boolean,
        store: KnownHostStore,
    ): KnownHost? =
        NativeBridge.nativeHostFingerprint(handle).takeIf { it.isNotEmpty() }
            ?.let { store.trust(host, port, name, it, paired) }
}

/**
 * The one session this process owns, and the one dial allowed to be in flight.
 *
 * Process-wide (null = not streaming), published by the composition that owns the stream.
 * `launchMode` is `standard`, so a `punktfunk://` link arrives as a second activity instance that
 * knows nothing about the first; static state is what crosses that gap, and the process dying
 * resets it.
 *
 * It also gates [connectToHost]. The console's native shell and its event pump outlive the screen
 * that raised them, so a `Launch` behind a live stream still reaches the dial: the host admits it
 * by `mode-conflict: JOIN` at the owner's mode and this side neither shows nor closes it. One
 * launch, one session.
 */
object SessionGate {
    /** The host a live stream is on. */
    data class Live(val hostId: String?)

    @Volatile
    var live: Live? = null

    private val dialing = AtomicBoolean(false)

    /** Claim the dial, or `false` when a session or another dial holds it. Pair with [release]. */
    fun take(): Boolean = live == null && dialing.compareAndSet(false, true)

    fun release() {
        dialing.set(false)
    }

    /** Session closes, off the UI thread: the QUIC close drains for up to 300 ms. */
    private val closer = Executors.newSingleThreadExecutor { r ->
        Thread(r, "pf-close").apply { isDaemon = true }
    }

    /** Close [handle] in the background. The next dial waits for it, so sessions never overlap. */
    fun close(handle: Long) = closer.execute { NativeBridge.nativeClose(handle) }

    /** Block until every queued close finished, bounded. Off the main thread. */
    fun awaitClosed() {
        runCatching { closer.submit {}.get(2, TimeUnit.SECONDS) }
    }
}

/**
 * The one place [NativeBridge.nativeConnect] is assembled — shared by [ConnectScreen], the library
 * launcher ([LibraryScreen]) and the console shell. Derives the mode / HDR / gamepad settings the
 * host needs from [settings]. [pinHex] is the pinned fingerprint (empty ⇒ TOFU). [launch] is a
 * store-qualified library id (`steam:<appid>` / `custom:<id>`) to boot straight into a game, or
 * `null` for the desktop. [dialer] names the shell and path that asked, for the log and the wire.
 *
 * Gated by [SessionGate]: a dial behind a live session, or beside one already in flight, returns
 * `0` without touching the host. Returns the session handle, or `0` on failure. Call off the main
 * thread.
 */
suspend fun connectToHost(
    context: Context,
    settings: Settings,
    identity: ClientIdentity,
    host: String,
    port: Int,
    pinHex: String,
    launch: String?,
    dialer: String,
    timeoutMs: Int = CONNECT_TIMEOUT_MS,
    preset: StreamPreset? = null,
    // [DELIVERY_PROBE_ONLY] for a network check; a plain connect streams.
    deliveryFlags: Int = 0,
    // The host profile to play as (its id); `null` sends none.
    profile: String? = null,
): Long {
    // One launch, one session: every shell's connect lands here, so the refusal lives here too.
    if (!SessionGate.take()) {
        Log.w("punktfunk", "dial refused ($dialer): this device already has a session or a dial in hand")
        return 0L
    }
    try {
        return dial(
            context, settings, identity, host, port, pinHex, launch, dialer, timeoutMs, preset,
            deliveryFlags, profile,
        )
    } finally {
        SessionGate.release()
    }
}

/** [connectToHost] with the gate already taken. */
private suspend fun dial(
    context: Context,
    settings: Settings,
    identity: ClientIdentity,
    host: String,
    port: Int,
    pinHex: String,
    launch: String?,
    dialer: String,
    timeoutMs: Int,
    preset: StreamPreset?,
    deliveryFlags: Int,
    profile: String?,
): Long {
    // Advertise HDR only when the user enabled it AND this device's display can present it (else the
    // host sends a proper SDR stream rather than PQ the panel would mis-tone-map).
    val (baseW, baseH, hz) = settings.effectiveMode(context)
    // Render scale: ask the host for `chosen mode × scale` (even + codec-clamped) — > 1 supersamples
    // (the compositor downscales the larger decoded frame to the SurfaceView), < 1 renders under
    // native. 1.0 leaves the resolved mode untouched.
    val (w, h) = RenderScale.apply(
        baseW, baseH, settings.renderScale, RenderScale.maxDimension(settings.codec)
    )
    val hdrEnabled = settings.hdrEnabled && displaySupportsHdr(context)
    // 10-bit on its own asks nothing of the PANEL: Main10 at BT.709 decodes and presents on an
    // ordinary display, and the compositor dithers what it cannot show. So this is the user's
    // setting alone, with no display probe — unlike HDR directly above it.
    val tenBitSdr = settings.tenBitSdr
    // "Automatic" resolves to a concrete pad type from the connected controller's VID/PID.
    val gamepadPref = Gamepad.resolvePref(settings.gamepad)
    // The requested audio format as the two Hello fields — `0`/`0` when the user chose Standard,
    // which is what keeps the lossless capability bit OFF (see `audioFormatWire`).
    //
    // Sent at every channel count, including surround. This used to be clamped to Opus on 5.1/7.1
    // because a lossless surround frame did not fit one QUIC datagram, but the frame ladder is
    // channel-aware: a 5.1 session simply negotiates a shorter frame (and pays for it in packet
    // rate) and 96/24 5.1 fits nothing and is declined. That is the host's decision to make with the
    // connection's real datagram size in hand, not one to pre-empt from here with an MTU this side
    // never measured.
    val (audioRateHz, audioBits) = settings.audioFormatWire()
    // NonCancellable: a cancelled withContext drops its result, and the dial cannot be
    // interrupted — the session would open with nobody to close it.
    val handle = withContext(Dispatchers.IO + NonCancellable) {
        SessionGate.awaitClosed() // the last stream's close, which its screen handed off
        // Transport-level half of "Low-latency mode (experimental)" (DSCP marking on the media
        // sockets) — must be applied before connect, since sockets are tagged at creation.
        NativeBridge.nativeSetLowLatencyMode(settings.lowLatencyMode)
        val multiSlice = VideoDecoders.multiSliceTolerant()
        val partialFrame = VideoDecoders.partialFrameCapable()
        // Slice-progressive delivery: decoder truth AND the low-latency toggle — parts are part of
        // the fast pipeline, and the toggle's "off" is the conservative configuration throughout.
        val frameParts = settings.lowLatencyMode && partialFrame
        val codecBits = VideoDecoders.decodableCodecBits()
        // Automatic codec (P5, measured NP3 ↔ RTX 4090): AV1 beat HEVC by ~1.2 ms end-to-end at
        // identical conditions, so under "Automatic" this device prefers AV1 when it hardware-
        // decodes it (the AV1 bit is only ever set for a real, non-blocked hardware decoder) AND
        // it lacks FEATURE_PartialFrame — a partial-frame device keeps HEVC, whose slice overlap
        // AV1 cannot ride (AV1 has no slices; the host's chunked poll never arms). The host
        // honors the preference only inside the probed shared codec set, so an AV1-less encoder
        // still resolves HEVC. An explicit user choice always wins unchanged.
        val preferredCodec = settings.preferredCodec().takeIf { it != 0 }
            ?: if (codecBits and 4 != 0 && !partialFrame) 4 else 0
        // PyroWave takes its rate from the player's quality. Asking Automatic (0) is what lets
        // the host fit that rate to the link before the first frame; a stored fixed rate is
        // kept for the other codecs.
        val pyrowave = preferredCodec == 8 && codecBits and 8 != 0
        // The connect-time capability readout (`adb logcat -s pf.caps`): the P2 slice pipeline
        // is client-inert unless BOTH probes pass — this line says which decoder failed one.
        Log.i(
            "pf.caps",
            VideoDecoders.capsReport() +
                " → multiSlice=$multiSlice parts=$frameParts prefer=$preferredCodec" +
                " (lowLatency=${settings.lowLatencyMode})",
        )
        val request = ConnectRequest(
            host = host, port = port, width = w, height = h, refreshHz = hz,
            certPem = identity.certPem, keyPem = identity.privateKeyPem, pinHex = pinHex,
            bitrateKbps = if (pyrowave) 0 else settings.bitrateKbps, compositorPref = settings.compositor,
            pyrowaveBppX100 = settings.pyrowaveBppX100(), gamepadPref = gamepadPref,
            hdrEnabled = hdrEnabled, tenBitSdr = tenBitSdr, multiSliceOk = multiSlice,
            framePartsOk = frameParts,
            audioChannels = settings.audioChannels,
            // The audio format this session asks for. Only ever a request: the host's own gate
            // may resolve it back to Opus, and the native side downgrades it first if AAudio on
            // this device will not open the rate — a rate the wire has committed to cannot be
            // rescued afterwards, so the fallback has to happen before the Hello.
            audioRateHz = audioRateHz, audioBits = audioBits,
            // What this device can decode (H.264 always; HEVC and AV1 when a real decoder exists) +
            // the soft codec preference (user choice, or the Automatic AV1 rule above) — the
            // host resolves the emitted codec from both.
            videoCodecs = codecBits, preferredCodec = preferredCodec, timeoutMs = timeoutMs,
            launch = launch,
            // The host's approval-list / trust-store label for this device — the same
            // user-set device name the pairing dialogs offer for nativePair.
            deviceName = deviceName(context),
            // Tier-A pad audio: ask for the 0xD1 plane only when a setting would render it, so a
            // user with it off does not make the host provision endpoints it will never feed.
            padAudioOk = settings.padHaptics || settings.padSpeaker,
            // "Keep host audio playing": the host taps its own default output rather than
            // silencing it for the session. Free to ask for — an older host just ignores it.
            keepHostAudio = settings.keepHostAudio,
            videoFit = settings.videoFit,
            // Which build, and which shell and path, opened this session — the host's
            // `handshake complete` `client=` field.
            dialer = "android ${appVersion(context)} $dialer",
            presetId = preset?.id,
            presetName = preset?.name,
            deliveryFlags = deliveryFlags,
            profile = profile,
        )
        NativeBridge.nativeConnect(request.toJson())
    }
    if (handle != 0L && !currentCoroutineContext().isActive) {
        withContext(Dispatchers.IO + NonCancellable) { NativeBridge.nativeClose(handle) }
    }
    currentCoroutineContext().ensureActive()
    return handle
}
