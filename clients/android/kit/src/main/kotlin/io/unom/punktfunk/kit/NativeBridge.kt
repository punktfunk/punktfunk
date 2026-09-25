package io.unom.punktfunk.kit

/**
 * The single JNI seam to `libpunktfunk_android.so` (the Rust-heavy client core).
 *
 * Symbols are implemented in `clients/android/native`. This object is intentionally thin —
 * all protocol logic lives in Rust (`punktfunk-core` + the connector); Kotlin only marshals.
 */
object NativeBridge {
    init {
        System.loadLibrary("punktfunk_android")
    }

    /** punktfunk-core C-ABI version. A successful call proves the native library is linked. */
    external fun abiVersion(): Int

    /** punktfunk-core crate version string. */
    external fun coreVersion(): String

    /**
     * Mint a fresh persistent self-signed identity, returned as
     * `"<certPem>\n-----PUNKTFUNK-KEY-----\n<keyPem>"`, or `""` on error. Kotlin persists it
     * (Keystore-wrapped via `IdentityStore`) and only calls this again when the store is empty.
     */
    external fun nativeGenerateIdentity(): String

    /**
     * Connect as [ConnectRequest.toJson] describes. Returns an opaque session handle, or `0` on
     * failure ([nativeTakeLastError] says why). Pair with exactly one [nativeClose].
     */
    external fun nativeConnect(requestJson: String): Long

    /** 64-hex SHA-256 of the cert the host presented on [handle]; valid after a successful connect. */
    external fun nativeHostFingerprint(handle: Long): String

    /**
     * Has the underlying QUIC session ended? `true` once the connection closed — a host suspend /
     * crash / network drop idle-timed it out (~8 s), or the host closed it — from then on no frame
     * ever arrives and the video sits frozen on its last one. The stream watchdog polls this (~1 Hz)
     * to leave a dead stream and return to the menu, where the user can Wake-on-LAN the host, instead
     * of stranding them on a frozen frame. `false` on a `0` handle. Cheap (one atomic load); UI-safe.
     */
    external fun nativeSessionEnded(handle: Long): Boolean

    /**
     * WHY the session ended, as a [SessionEndReason] ordinal — decode with
     * [SessionEndReason.fromNative]. `0` (NONE) before it ends, or on a `0` handle.
     *
     * The companion to [nativeSessionEnded], which only says THAT it ended. Both are needed: the
     * flag to leave a dead stream, this to decide what to tell the user. A player quitting their
     * game and a host falling off the network both end the session, and with no way to separate
     * them the watchdog said "the host may be asleep" for all of them — wrong for every deliberate
     * ending. Cheap (one atomic load); UI-safe.
     */
    external fun nativeEndReason(handle: Long): Int

    /**
     * The session's live access state as `[grants, remainingSecs, updateSeq]`, or `null` on a `0`
     * handle. `grants` is a [SessionAccess] bitmask; `remainingSecs` counts down to the access
     * expiry (`0` = permanent); `updateSeq` increments once per `AccessUpdate` the host sent
     * (latest-wins — the state IS the fold, this counter is how a poller tells a fresh T−5 m /
     * T−1 m warning arrived and owes a toast). Seeded from the Welcome's access advert; an old
     * host — or an old native lib — reads as full control, permanent, exactly what such a host
     * enforces. Poll ~1 Hz alongside [nativeSessionEnded]. Cheap; safe on the UI thread.
     */
    external fun nativeAccessState(handle: Long): IntArray?

    /**
     * Run the SPAKE2 PIN ceremony, presenting [certPem]/[keyPem]. Returns the host's verified
     * fingerprint (64-hex) to persist + pin, or `""` on failure (wrong PIN / MITM / unreachable).
     * Blocking — call off the main thread.
     */
    external fun nativePair(
        host: String,
        port: Int,
        certPem: String,
        keyPem: String,
        pin: String,
        name: String,
    ): String

    /**
     * The native client's recent log ring rendered as one text bundle, oldest first,
     * prefixed by [header] (this app's identity line) — the body for "Send logs to host"
     * (`POST /api/v1/client-logs` over the same mTLS client the library fetch uses).
     * Never empty; cheap (string copy, no I/O).
     */
    external fun nativeRenderLogs(header: String): String

    /**
     * One `pf.wifi` line in the log ring above: why it was written, then the Wi-Fi link's signal
     * (dBm), transmit and receive rates (Mb/s), channel frequency (MHz) and
     * `WifiInfo.getWifiStandard()`. `-1` is unknown; a frequency ≤ 0 means off Wi-Fi.
     */
    external fun nativeLogWifiLink(
        reason: String,
        rssiDbm: Int,
        txMbps: Int,
        rxMbps: Int,
        freqMhz: Int,
        standard: Int,
    )

    /** One `pf.display` line in the log ring above, as written: the displays and fold features. */
    external fun nativeLogDisplay(line: String)

    /**
     * The machine token of the most recent failed [nativeConnect]/[nativePair], cleared on read
     * (`""` when none) — call right after a `0` handle / `""` fingerprint. A typed host rejection
     * yields its wire token ("not-armed", "denied", "approval-timeout", "superseded", "busy",
     * "rate-limited", "bound-other", "identity-required", "wire-version"); transport-level causes
     * yield "crypto" (wrong PIN / identity mismatch), "timeout", "io", or "error". Lets the UI say
     * WHY instead of the old catch-all that blamed the PIN for dead network paths.
     */
    external fun nativeTakeLastError(): String

    /**
     * Signal a **deliberate** user disconnect on [handle] before [nativeClose]: the session closes
     * with `QUIT_CLOSE_CODE` so the host tears it down immediately instead of holding the keep-alive
     * linger for a reconnect. Call from an explicit disconnect gesture only — NOT from a
     * host-ended/network-drop end or an app-background (those keep the linger). No-op on `0`.
     */
    external fun nativeDisconnectQuit(handle: Long)

    /**
     * Ask the host to switch the live session to [width]×[height] at [refreshHz] with no reconnect.
     * Non-blocking: on acceptance the stream continues at the new mode from a keyframe that carries
     * its own parameter sets; a rejection leaves the session unchanged. `false` when the request
     * could not be queued (a `0` handle, a closed session, or a non-positive dimension).
     */
    external fun nativeRequestMode(handle: Long, width: Int, height: Int, refreshHz: Int): Boolean

    /** Tear down a session handle returned by [nativeConnect]. No-op on `0`. */
    external fun nativeClose(handle: Long)

    // ---- LAN discovery: mDNS browse of `_punktfunk._udp` in Rust (mdns-sd), polled by Kotlin ----
    // Replaces NsdManager. The caller holds the Wi-Fi MulticastLock for the browse lifetime; raw
    // multicast *reception* needs it. See io.unom.punktfunk.kit.discovery.HostDiscovery.

    /**
     * Start browsing `_punktfunk._udp` on the LAN. Returns an opaque discovery handle, or `0` on
     * failure. Pair with exactly one [nativeDiscoveryStop]. Cheap + non-blocking (spawns the mDNS
     * daemon + a fold thread).
     */
    external fun nativeDiscoveryStart(): Long

    /**
     * Put a fresh query on the wire for [handle] and reset the doubling re-query backoff, keeping
     * the daemon. This is the rescan: rebuilding the daemon re-binds :5353 and re-joins the
     * multicast groups, and a rebuild that fails leaves the device with no discovery at all.
     */
    external fun nativeDiscoveryRescan(handle: Long)

    /**
     * The current resolved-host snapshot for [handle]: newline-joined records, each
     * `key␟name␟addr␟port␟fp␟pair␟mac` (`␟` = U+001F). Empty string = no hosts / `0` handle. Poll ~1 Hz;
     * cheap (a lock + string build), safe to call on the main thread.
     */
    external fun nativeDiscoveryPoll(handle: Long): String

    /** Stop the browse, shut the mDNS daemon down and join its thread. No-op on `0`. */
    external fun nativeDiscoveryStop(handle: Long)

    /**
     * Send a Wake-on-LAN magic packet to wake a sleeping host. [macsCsv] is comma-separated MAC
     * addresses (`aa:bb:..,cc:dd:..`), learned from the host's mDNS `mac` TXT while it was online;
     * [lastIp] is the host's last-known IPv4 (or empty). Returns true if at least one datagram was
     * sent. No handle — callable without a live session. Do NOT call on the main thread (it does
     * blocking socket sends); run it on a background dispatcher.
     */
    external fun nativeWakeOnLan(macsCsv: String, lastIp: String): Boolean

    /**
     * Bounded QUIC reachability probe to [host]:[port] (mDNS-independent): the lowercase-hex
     * SHA-256 of the certificate that answered within [timeoutMs], or `null` if nothing did.
     * Lets a saved host reached over a routed network (Tailscale/VPN/another subnet) — which
     * never advertises on mDNS — still show as online.
     *
     * The handshake is unpinned, so the answer names whoever holds the address, not necessarily
     * your host: compare it with [io.unom.punktfunk.kit.discovery.Presence.isSelf]. Blocking
     * (builds its own runtime) — run on a background dispatcher, never the main thread.
     */
    external fun nativeProbe(host: String, port: Int, timeoutMs: Int): String?

    /**
     * Start a bandwidth speed test on [handle]: the host bursts filler over the real data plane at
     * [targetKbps] of goodput for [durationMs] (each clamped host-side to ≤ 3 Gbps / ≤ 5 s),
     * **briefly pausing video**. Measuring over the stream's own path is the point — the answer is
     * about the link this host's stream will take, not about generic throughput.
     *
     * Non-blocking: poll [nativeProbeResult] until it reports done. Starting a probe resets any
     * prior measurement. Returns false on a dead handle. Cheap; safe on the main thread.
     */
    external fun nativeSpeedTest(handle: Long, targetKbps: Int, durationMs: Int): Boolean

    /**
     * The current speed-test measurement, partial until `[0] != 0.0`:
     * `[done, throughputKbps, lossPct, hostDropPct, elapsedMs, recvBytes]`. Zeros before any
     * probe, null on a dead handle. Cheap (one lock + a copy); safe to poll on the main thread.
     */
    external fun nativeProbeResult(handle: Long): DoubleArray?

    /**
     * Apply the user's "Low-latency mode (experimental)" toggle to the process-wide transport
     * defaults — today just DSCP/QoS marking on the media sockets. Must be called BEFORE
     * [nativeConnect] (the tag is applied at socket creation); `HostConnect.connectToHost` does.
     * The rest of the toggle rides explicit per-session parameters ([nativeStartVideo] /
     * [nativeStartAudio]). Cheap (one atomic store); UI-safe.
     */
    external fun nativeSetLowLatencyMode(enabled: Boolean)

    /**
     * The MediaCodec MIME the host resolved for this session (`"video/hevc"` / `"video/avc"` /
     * `"video/av01"`), or `""` on a `0` handle. Kotlin ranks `MediaCodecList` decoders for this
     * MIME (see [io.unom.punktfunk.kit.VideoDecoders]) before [nativeStartVideo]. Cheap; UI-safe.
     */
    external fun nativeVideoMime(handle: Long): String

    /**
     * The negotiated video mode as `[width, height, refreshHz]`, or `null` on a `0` handle.
     * Resolved at the handshake, so it is known before the first frame — the stream view sizes
     * itself to THIS aspect rather than stretching the picture to the panel's, and pins the
     * panel's display mode to the stream refresh. The trailing `refreshHz` was appended later
     * (an older native lib returns only `[width, height]` — index defensively). Follows an
     * accepted [nativeRequestMode] switch once the host's ack lands. Cheap; UI-safe.
     */
    external fun nativeVideoSize(handle: Long): IntArray?

    /**
     * A short human label for the codec the host resolved (`"H.264"` / `"HEVC"` / `"AV1"` /
     * `"PyroWave"`), for the stats HUD's video-feed line, or `""` on a `0` handle. Distinct from
     * [nativeVideoMime] because the MIME collapses PyroWave onto `video/hevc` and can't name it.
     * Fixed for the session (resolved at the handshake); read once. Cheap; UI-safe.
     */
    external fun nativeVideoCodecLabel(handle: Long): String

    /**
     * Whether this device's GPU can decode PyroWave (the wired-LAN wavelet codec): a Vulkan 1.3
     * device with the compute feature set its kernels need. Session-independent — it asks the
     * driver, not a host — and folded into [VideoDecoders.decodableCodecBits] so the host never
     * emits a codec nothing here can decode.
     *
     * Unlike every other codec on this client, PyroWave is not a `MediaCodec`: it decodes as
     * Vulkan compute and presents through its own swapchain on the same `SurfaceView`. Always
     * false on the 32-bit ABI, where the codec is not built. The first call creates and destroys
     * a Vulkan instance and native caches the answer for the process — do that one off the main
     * thread; later calls are free.
     */
    external fun nativePyrowaveCapable(): Boolean

    /**
     * Start the decode thread rendering onto [surface] (a SurfaceView's surface). Decode runs
     * entirely in Rust (NDK AMediaCodec → ANativeWindow) — no per-frame JNI. [decoderName] is the
     * decoder Kotlin ranked from `MediaCodecList` (`""` = let the platform resolve the default for
     * the MIME — what the pre-overhaul client always did); [lowLatencyMode] is the user's
     * "Low-latency mode" master toggle (ON by default: per-SoC tuning + thread boosts; off runs
     * the same loop with plain keys, the per-device escape hatch); [lowLatencyFeature] is whether
     * [decoderName] advertised `FEATURE_LowLatency` (HUD label only). [isTv] drives an active HDMI
     * mode switch to the stream refresh on TV boxes when the toggle is on (vs. the softer seamless
     * hint otherwise). [presentPriority]/[smoothBuffer] are the timeline presenter's intent
     * (0 = lowest latency / 1 = smoothness; buffer 0 = automatic, else 1..3 frames) — the Apple
     * client's `present_priority`/`smooth_buffer` pair. No-op if already started.
     */
    external fun nativeStartVideo(
        handle: Long,
        surface: android.view.Surface,
        decoderName: String,
        lowLatencyMode: Boolean,
        lowLatencyFeature: Boolean,
        isTv: Boolean,
        presentPriority: Int,
        smoothBuffer: Int,
        /** The display mode's own refresh rate (0 = unknown) — the latch grid the presenter
         *  subdivides onto when the platform down-rates the app's choreographer stream. */
        panelFps: Int,
        /** The video SurfaceView's on-screen pixel size (0 = not laid out yet). The ASurfaceControl
         *  present backend composites its layer in this coordinate space — the aspect-fitted display
         *  footprint — rather than the window's rotated/scaled buffer geometry. */
        surfaceW: Int,
        surfaceH: Int,
    )

    /**
     * Re-report the video SurfaceView's on-screen pixel size — call it from every `surfaceChanged`.
     *
     * The ASurfaceControl present backend composites the picture into exactly this rectangle, and
     * the view grows AFTER [nativeStartVideo] has run: the stream screen hides the system bars and
     * switches the window to draw into the display cutout a frame or two later, and neither
     * recreates the surface. Without this the layer keeps painting at its start-up size in the
     * corner of a now-bigger surface. Non-positive values are ignored. No-op on a `0` handle;
     * cheap (one atomic store), UI-safe.
     */
    external fun nativeVideoSurfaceSize(handle: Long, width: Int, height: Int)

    /**
     * The visible part of the frame, as fractions ([VideoPlacement.srcX] over the frame width
     * and so on). The SurfaceView is laid out at the picture's rect; this carries what that size
     * cannot, the edges Crop to fill cuts off. Out-of-range values reset to the full frame.
     * No-op on a `0` handle; one atomic store, UI-safe.
     */
    external fun nativeVideoSourceCrop(handle: Long, left: Float, top: Float, right: Float, bottom: Float)

    /**
     * The decoder's picture size as `[width, height]`, or `null` before its first output format.
     * Differs from [nativeVideoSize] when the host frames the picture for this device (a join, a
     * mirrored head). One atomic load; UI-safe.
     */
    external fun nativeVideoDecodedSize(handle: Long): IntArray?

    /** Stop + join the decode thread without closing the session. No-op on `0`. */
    external fun nativeStopVideo(handle: Long)

    /**
     * The background keep-alive's video drain: pop access units and discard them while no decode
     * thread runs. Without it the frame queue stands, the client jumps to live and asks the host
     * for a keyframe every two seconds for as long as the app is away. Idempotent; no-op on `0`.
     */
    external fun nativeVideoDrain(handle: Long, on: Boolean)

    /**
     * Close ~1 s of the stats overlay window and return it formatted for [tier] (0 Off, 1 Compact,
     * 2 Normal, 3 Detailed) in the Advanced vocabulary when [advanced]: one `<role>\t<text>` per
     * line, or `null` when no decode thread runs. [panelHz] is the rate this app may render at and
     * [panelModeHz] the panel's active mode (0 = unknown); [preset] closes the first line. Poll
     * ~1 Hz; each call closes the window.
     */
    external fun nativeVideoStatsLines(
        handle: Long,
        tier: Int,
        advanced: Boolean,
        panelHz: Float,
        panelModeHz: Float,
        preset: String?,
    ): String?

    /**
     * Gate per-frame stats sampling on the HUD being visible: while disabled the decode thread
     * skips the per-AU clock read + lock, so toggle this with the overlay (and only poll
     * [nativeVideoStatsLines] while it's on). Enabling resets the measurement window — no stale data.
     * Sticky for the session (survives video stop/start). No-op on `0`.
     */
    external fun nativeSetVideoStatsEnabled(handle: Long, enabled: Boolean)

    /**
     * Start host→client audio: Opus decode → jitter ring → AAudio, all in Rust.
     * [lowLatencyMode] (the experimental toggle) additionally tags the stream usage=Game for the
     * HAL's game-audio routing. No-op if already started. Best-effort — a failure leaves video
     * streaming.
     *
     * [isTv] steers the AAudio open ladder: a TV box starts at Shared rather than betting the
     * audio plane on an Exclusive/MMAP path whose routing we cannot verify from inside the
     * process. Passed from `FEATURE_LEANBACK` (same source as [nativeStartVideo]) because the
     * native side's own `ro.build.characteristics` check is not answered by every TV device.
     */
    external fun nativeStartAudio(handle: Long, lowLatencyMode: Boolean, isTv: Boolean)

    /** Stop + join the audio thread and close AAudio, without closing the session. No-op on `0`. */
    external fun nativeStopAudio(handle: Long)

    /**
     * Start mic uplink: AAudio input → Opus (48 kHz mono, 10 ms) → host (`send_mic` / 0xCB), all in
     * Rust. [echoCancel] opens the capture under the VoiceCommunication preset (the HAL's own echo
     * canceller / noise suppressor) and allocates an audio session id; the return value is that id
     * (`> 0`) so the caller can attach the Java [android.media.audiofx.AcousticEchoCanceler] /
     * [android.media.audiofx.NoiseSuppressor] as a backstop — `0` when none was allocated
     * (echoCancel off, the device refused the preset and the open fell back to the plain path, or
     * the mic failed entirely). No-op if already running (returns the running capture's id). The
     * caller MUST hold RECORD_AUDIO; otherwise the AAudio input stream fails to open and the rest
     * of the session keeps streaming.
     */
    external fun nativeStartMic(handle: Long, echoCancel: Boolean): Int

    /**
     * Stop + join the mic thread and close the AAudio input stream. No-op on `0`. Leaves the
     * session's mute state ([nativeSetMicMuted]) alone — a surface recreate stops and restarts the
     * mic, and a user who muted must stay muted through it.
     */
    external fun nativeStopMic(handle: Long)

    /**
     * Mute/unmute the mic uplink mid-stream. Muting does NOT stop the capture: the AAudio input
     * stream, the input preset it settled on and its primed buffers stay as they are, and the
     * encode loop drops each 10 ms frame instead of encoding + sending it — so room audio is never
     * encoded and nothing goes on the wire, while a toggle costs an atomic store and takes effect
     * on the next 10 ms boundary (a stop/start would re-run the preset fallback ladder and re-prime
     * buffers every time).
     *
     * Sticky for the SESSION — the flag lives on the handle, not on the capture — so the mic
     * restart a surface recreate performs comes back muted, with no window for an unmuted frame to
     * escape; a fresh session always starts unmuted. Nothing here is persisted. No-op on `0`.
     * Cheap (one atomic store); UI-safe.
     *
     * One honest consequence of keeping the stream open: the platform's own recording indicator
     * stays lit while muted, because the mic really is still open. What stops is the encode and the
     * send — no captured audio leaves the process.
     */
    external fun nativeSetMicMuted(handle: Long, muted: Boolean)

    /**
     * Silence this device's speakers. Local: nothing reaches the host, so a second client joined
     * to the same display keeps hearing the game. Audio keeps arriving and keeps decoding — only
     * what is queued for playback is zeroed — so the decoder holds its state and unmute lands in
     * step instead of re-priming the ring. Per session, never persisted. No-op on `0`.
     */
    external fun nativeSetStreamMuted(handle: Long, muted: Boolean)

    /**
     * Why this session is silent: `1` this device, `2` the host's own per-session mute, `3` both,
     * `0` audible. Name the reason in the overlay from this — a local unmute leaves an operator
     * mute standing. `0` on a `0` handle.
     */
    external fun nativeAudioMute(handle: Long): Int

    /**
     * Start tier-A DualSense pad audio: render the host's `0xD1` streams on the pad's own
     * 4-channel USB audio device.
     *
     * [fd] is an open [android.hardware.usb.UsbDeviceConnection]'s file descriptor. Native code
     * **borrows** it — it claims the pad's audio interface through usbfs (which leaves any HID
     * claim on the same device alone) and never closes the descriptor. The caller must keep the
     * connection open until [nativeStopPadAudio] returns.
     *
     * This also declares the pad's render capability to the host; without it no `0xD1` is sent.
     *
     * Returns false when there is nothing to render. A kernel that refuses the interface claim is
     * NOT reported here — the renderer discovers that on its own thread and the session simply
     * carries on without tier A, because some OEM kernels refuse and no app-side fix exists.
     */
    external fun nativeStartPadAudio(
        handle: Long,
        pad: Int,
        fd: Int,
        haptics: Boolean,
        speaker: Boolean,
    ): Boolean

    /**
     * Stop tier-A pad audio and join its render thread, and hand the pad back to wire rumble.
     *
     * Returns only once the thread is joined — so the `UsbDeviceConnection` may be closed as soon
     * as this returns, and not before.
     */
    external fun nativeStopPadAudio(handle: Long, pad: Int)

    /**
     * Drive the pad with a test tone through the real render path — no host, no session.
     *
     * [fd] must come from a connection **nothing else is driving transfers on**: two engines on
     * one usbfs descriptor reap each other's completions. Blocks for roughly [seconds]; run it off
     * the main thread. Returns sample frames written, or negative on failure.
     */
    external fun nativePadAudioSelfTest(fd: Int, seconds: Int, hz: Int): Int

    /**
     * Is a mic capture actually RUNNING — i.e. did [nativeStartMic] open a stream, and has
     * [nativeStopMic] not been called since? Offer the in-stream mute control on THIS rather than
     * on the user's setting: a device that refused every AAudio input rung (or a missing
     * RECORD_AUDIO grant) then shows no control instead of a lie about a mic being heard. `false`
     * on a `0` handle. Cheap; UI-safe.
     */
    external fun nativeMicActive(handle: Long): Boolean

    // ---- Input: Kotlin captures, Rust forwards to the host (send_input) ----

    /** Relative mouse move; dx/dy are device-pixel deltas (screen +y down). */
    external fun nativeSendPointerMove(handle: Long, dx: Int, dy: Int)

    /**
     * Absolute mouse position — the host moves the cursor to (x, y) in a [surfaceWidth]×[surfaceHeight]
     * pixel space (it normalizes against that size and maps into the output region). Touch
     * "direct pointing": the cursor jumps to the finger. Parity with the Apple client's absolute touch.
     */
    external fun nativeSendPointerAbs(handle: Long, x: Int, y: Int, surfaceWidth: Int, surfaceHeight: Int)

    /** One mouse-button transition. button: 1=left 2=middle 3=right 4=X1 5=X2. */
    external fun nativeSendPointerButton(handle: Long, button: Int, down: Boolean)

    /** One scroll step. axis: 0=vertical 1=horizontal. delta: signed, 120-scaled, +=up/right.
     *  Legacy embedder API — production capture uses [nativeSendNormalizedScroll]. */
    external fun nativeSendScroll(handle: Long, axis: Int, delta: Int, precise: Boolean)

    /**
     * One normalized scroll step (`InputKind::Scroll`). [axis]: 0=vertical 1=horizontal. [delta]:
     * signed Q24.8 in the source's unit — 120-per-detent for Wheel/Unknown, DIP for the rest.
     * [source]/[phase] are the wire bytes; Rust drops a malformed pair before anything is sent.
     */
    external fun nativeSendNormalizedScroll(handle: Long, axis: Int, delta: Int, source: Int, phase: Int)

    /** Live natural-scroll toggle, applied once at the core's outbound seam. False on a dead session. */
    external fun nativeSetInvertScroll(handle: Long, invert: Boolean): Boolean

    /**
     * One REAL touchscreen transition (the touch-passthrough input mode). [kind]: 0=down 1=move
     * 2=up. [id] distinguishes fingers and is reusable after up; coordinates are pixels on the
     * client's touch surface — the host rescales against [surfaceWidth]×[surfaceHeight] and
     * injects a real touch contact. On up only [id] matters.
     */
    external fun nativeSendTouch(
        handle: Long,
        id: Int,
        kind: Int,
        x: Int,
        y: Int,
        surfaceWidth: Int,
        surfaceHeight: Int,
    )

    /** One key transition. vk: Windows VK (0 = dropped by Rust). mods: VK modifier mask (0 for now). */
    external fun nativeSendKey(handle: Long, vk: Int, down: Boolean, mods: Int)

    /**
     * Whether the host advertised full-fidelity stylus injection (`HOST_CAP_PEN`) — the gate
     * for splitting stylus pointers out of the touch path onto the pen plane. False on `0`.
     */
    external fun nativeHostSupportsPen(handle: Long): Boolean

    /**
     * Whether the host advertised touch injection (`HOST_CAP2_TOUCH`). Without it a passthrough
     * contact lands nowhere, so the stream runs the trackpad model instead and says so. False on
     * `0` and toward an older host.
     */
    external fun nativeHostSupportsTouch(handle: Long): Boolean

    /**
     * One stylus batch of STATE-FULL samples (the pen plane; design/pen-tablet-input.md §7):
     * [count] × 10 floats, oldest first — `[state, tool, x, y, pressure, distance, tilt_deg,
     * azimuth_deg, roll_deg, dt_us]`. `state` = the wire in-range/touching/barrel bits; `tool`
     * 0=pen 1=eraser; x/y/pressure/distance normalized 0..1; distance/tilt/azimuth/roll < 0 =
     * unknown. Send only when [nativeHostSupportsPen]; repeat the last sample ≤100 ms while the
     * pen is in range (the host force-releases a silent stroke after 200 ms).
     */
    external fun nativeSendPen(handle: Long, samples: FloatArray, count: Int)

    /**
     * Whether the host advertised committed-text injection (`HOST_CAP_TEXT_INPUT`) — its inject
     * backend can type Unicode text directly. Picks the real IME `InputConnection` (autocorrect,
     * gesture typing, non-Latin scripts) over the TYPE_NULL raw-key fallback. False on `0`.
     */
    external fun nativeTextInputSupported(handle: Long): Boolean

    /**
     * Committed IME text → one `TextInput` wire event per Unicode scalar, in order. Control
     * characters are skipped natively (Enter/Backspace ride [nativeSendKey]). Only meaningful
     * when [nativeTextInputSupported] returned true — older hosts ignore the events.
     */
    external fun nativeSendText(handle: Long, text: String)

    // ---- Shared clipboard (text v1): Kotlin drives ClipboardManager, Rust the protocol ----
    // Opt-in per session (nativeClipControl). Local copies are announced as lazy offers; bytes
    // cross only when the host pastes (a "fetch:" event answered by nativeClipServeText). Host
    // copies arrive as "offer:" events, fetched eagerly into the system clipboard.

    /**
     * The management-API port the host reported in this session's `Welcome` — where its game
     * library is served — or 0 if it advertised none (older host, or no management API).
     *
     * Persist it on the host record: unlike the mDNS `mgmt` TXT, this arrives over the connection
     * we have already authenticated, so it is what makes a host that moved off 47990 browsable
     * over a VPN, a routed subnet, or when it was added by address.
     */
    external fun nativeHostMgmtPort(handle: Long): Int

    /** Whether the host advertised a working shared-clipboard service (HOST_CAP_CLIPBOARD). */
    external fun nativeClipSupported(handle: Long): Boolean

    /** Session-level clipboard opt-in/out; nothing happens until enabled=true crosses. */
    external fun nativeClipControl(handle: Long, enabled: Boolean)

    /** Announce "this device's clipboard now holds text". [seq]: monotonic, newest wins. */
    external fun nativeClipOfferText(handle: Long, seq: Int)

    /** Pull the text of the host's offer [seq] → transfer id echoed on "data:"/"error:", or -1. */
    external fun nativeClipFetchText(handle: Long, seq: Int): Int

    /** Answer a "fetch:" event with the clipboard's current text (the host is pasting). */
    external fun nativeClipServeText(handle: Long, reqId: Int, text: String)

    /** Abort a clipboard transfer by id (either direction). */
    external fun nativeClipCancel(handle: Long, id: Int)

    /**
     * Block ≤250 ms for the next clipboard event, as a compact string: `state:<0|1>` ·
     * `offer:<seq>:<hasText>` · `fetch:<reqId>` · `data:<xferId>:<text>` · `cancel:<id>` ·
     * `error:<id>:<code>` · `closed` (session gone) — null on timeout. Dedicated poll thread.
     */
    external fun nativeNextClip(handle: Long): String?

    // ---- Gamepad: each controller forwarded on its own wire pad index (0..15, low byte of flags) ----
    // The pad index is assigned per Android device by GamepadRouter; a single controller lands on 0,
    // so its wire is byte-identical to the old single-pad path. The core folds the per-transition
    // events into seq'd GamepadState snapshots keyed on this index and owns the per-pad seq.

    /** One gamepad button transition on wire pad [pad] (0..15). bit: a [Gamepad].BTN_* bit. down: press/release. */
    external fun nativeSendGamepadButton(handle: Long, bit: Int, down: Boolean, pad: Int)

    /** One gamepad axis update on wire pad [pad] (0..15). axisId: [Gamepad].AXIS_* (0..5). value: stick i16 (+y=up) / trigger 0..255. */
    external fun nativeSendGamepadAxis(handle: Long, axisId: Int, value: Int, pad: Int)

    /**
     * Controller mouse on the wire pads in [mask] (bit = pad index): their buttons and sticks drive
     * the host pointer and a few keys while the host pad sits neutral. `0` returns every pad to
     * passthrough. False when the session is gone or the host did not grant pointer input.
     */
    external fun nativeSetPadMouse(handle: Long, mask: Int): Boolean

    /** The pads in controller mouse now. A removed pad or a lost pointer grant clears its bit. */
    external fun nativePadMouse(handle: Long): Int

    /**
     * Declare the controller KIND presented on wire pad [pad] (0..15) so the host builds a matching
     * virtual device (mixed types across pads). pref: a [Gamepad].PREF_* wire byte. Send ONCE when a
     * pad opens, BEFORE any of its input; an older host ignores it (that pad then uses the handshake's
     * session-default kind — the pre-existing single-pad behaviour on pad 0).
     */
    external fun nativeSendGamepadArrival(handle: Long, pref: Int, pad: Int)

    /** Signal wire pad [pad] (0..15) was unplugged so the host tears its virtual device down. The core stamps the seq + re-sends. */
    external fun nativeSendGamepadRemove(handle: Long, pad: Int)

    /**
     * Whether motion sent for a pad that declared [declaredPref] (the [Gamepad].PREF_* byte passed
     * to [nativeSendGamepadArrival]) can actually reach the game, or would be decoded and dropped
     * by a host backend without a motion plane — the X-Box classes have no gyro in their HID
     * contract.
     *
     * Answered natively, off `punktfunk_core::config::pad_motion_reaches`, rather than
     * reconstructed here from the session's requested/resolved prefs. The rule is subtler than it
     * looks (the host builds each pad from its OWN declaration and folds what it cannot build, so
     * neither the declaration nor the session echo answers it alone) and every way of getting it
     * wrong is silent, so it lives in one place with one set of tests.
     *
     * Ask ONCE when a pad opens, not per sample. `true` when the session handle is dead — "don't
     * suppress" is the safe answer whenever we cannot tell.
     */
    external fun nativePadMotionReaches(handle: Long, declaredPref: Int): Boolean

    /**
     * One raw HID input report from a client-captured controller (the as-is Steam Controller 2
     * passthrough), forwarded verbatim on the rich-input plane. [buf] is a DIRECT ByteBuffer whose
     * first [len] bytes are the report, id byte first (0x42/0x45/0x47 state, 0x43 battery, …);
     * len is clamped to 64. Called from the capture thread at the controller's own report rate.
     */
    external fun nativeSendPadHidReport(handle: Long, pad: Int, buf: java.nio.ByteBuffer, len: Int)

    /**
     * One touchpad contact from a client-captured controller (the Sony USB capture), forwarded on
     * the rich-input plane (`RichInput::Touchpad`). [finger] is the contact slot (0/1); [x]/[y]
     * are normalized 0..65535 in SCREEN convention (+y down — the wire's fixed meaning); active
     * false lifts the finger. Send on change only — the host holds per-slot state.
     */
    external fun nativeSendPadTouch(handle: Long, pad: Int, finger: Int, active: Boolean, x: Int, y: Int)

    /**
     * One motion-sensor sample from a client-captured controller (`RichInput::Motion`): gyro
     * pitch/yaw/roll + accel, each a raw signed-16 value in the pad's own units — the host passes
     * them straight into the virtual DualSense report. Called at the pad's report rate.
     */
    external fun nativeSendPadMotion(
        handle: Long,
        pad: Int,
        gyroPitch: Int,
        gyroYaw: Int,
        gyroRoll: Int,
        accelX: Int,
        accelY: Int,
        accelZ: Int,
    )

    // ---- Host→client gamepad feedback: Rust pulls block ~100ms, Kotlin renders (see GamepadFeedback) ----

    /**
     * Block up to ~100 ms for the next rumble update. Returns a packed positive long: bits 49..52 =
     * wire pad index (0..15), bit 48 = has a v2 lease, bits 32..47 = ttl_ms, bits 16..31 = low, bits
     * 0..15 = high (each amplitude 0..0xFFFF; 0/0 = stop), or -1 on timeout / session closed. Kotlin
     * routes the update to the controller holding that pad index. Call from a dedicated poll thread.
     */
    external fun nativeNextRumble(handle: Long): Long

    /**
     * Block up to ~100 ms for the next HID-output event, written into [buf] (a direct ByteBuffer,
     * capacity >= 128) as `[pad][kind][fields…]` (leading pad = the wire pad index to route to):
     * Led=pad 01 r g b, PlayerLeds=pad 02 bits, Trigger=pad 03 which effect…, raw as-is
     * passthrough report=pad 05 kind report-bytes (kind 0 = output report, 1 = feature report),
     * MicLed=pad 07 mode.
     * Returns the byte count, or -1 on timeout / session closed.
     */
    external fun nativeNextHidout(handle: Long, buf: java.nio.ByteBuffer): Int

    // ---- The Skia console UI (crates/pf-console-ui over EGL/GLES — clients/android/native/src/console) ----
    //
    // The same console shell the Linux/Windows session binary shows, drawn by native onto a
    // SurfaceView. Kotlin keeps the services and feeds the console's models as JSON in the model
    // types' own serde shape (HostRow, LibraryGame, ConsoleCmd, OverlayAction, Settings — see
    // `crates/pf-console-ui/src/model.rs` and `pf-client-core/src/trust.rs`); what the console
    // raises comes back through [nativeConsoleNextEvent]. Every call is main-thread-safe and cheap
    // except the two polls, which block ~100 ms and belong on their own threads.

    /**
     * Whether this `.so` carries the console host at all. Present on EVERY ABI — armeabi-v7a has no
     * prebuilt Skia archive yet, so there the rest of these symbols DO NOT EXIST and calling one is
     * an UnsatisfiedLinkError. Ask this first.
     */
    external fun nativeConsoleAvailable(): Boolean

    /**
     * Build the console: [optionsJson] = `{device_name, gpu_cache_bytes, settings: <trust::Settings>,
     * presets: [[id, name]], known_hosts: <KnownHosts>, entry: {} | {"library": <HostRow>}}`.
     * Returns a handle (its render thread parked until a surface arrives), or `0` on a bad options
     * document. EGL/Skia failures arrive later as a `{"dead": …}` event.
     */
    external fun nativeConsoleCreate(optionsJson: String): Long

    /** Stop the render thread (joined) and free. Stop + join the event poll thread FIRST. */
    external fun nativeConsoleDestroy(handle: Long)

    /** The SurfaceView's surface is up. */
    external fun nativeConsoleSurfaceCreated(handle: Long, surface: android.view.Surface)

    /** The surface's size changed. */
    external fun nativeConsoleSurfaceChanged(handle: Long)

    /** BLOCKS until the render thread has let go of the surface — call from `surfaceDestroyed`. */
    external fun nativeConsoleSurfaceDestroyed(handle: Long)

    /** Safe-area insets in surface pixels + the design-unit scale (`0` = the shell's own formula). */
    external fun nativeConsoleSetViewport(
        handle: Long,
        left: Float,
        top: Float,
        right: Float,
        bottom: Float,
        scale: Float,
    )

    /**
     * The raw pad, whenever it changes: [buttons] bit i = a, b, x, y, l1, r1 held; [lx]/[ly] the left
     * stick in wire units (±32767, +y down); [dpad] bit i = up, down, left, right held. Native runs
     * the shared menu synthesizer over it (dead zone, repeat, hysteresis).
     */
    external fun nativeConsolePadSample(handle: Long, buttons: Int, lx: Int, ly: Int, dpad: Int)

    /**
     * A discrete menu event: 0..3 move up/down/left/right, 4 confirm, 5 back, 6 secondary (Y),
     * 7 tertiary (X), 8 jump back (L1), 9 jump forward (R1), 10/11 a remote's OK down/up (acts on
     * release; held, it opens the card's menu). For input that is already an event on this side
     * (a TV remote's D-pad keys, the touch legend).
     */
    external fun nativeConsoleMenu(handle: Long, event: Int)

    /**
     * Pointer input in surface pixels: [kind] 0 move, 1 primary down, 2 primary up, 3 secondary
     * down (= Back), 4 wheel ([dy] steps, + = up), 5 cancel.
     */
    external fun nativeConsolePointer(handle: Long, kind: Int, x: Float, y: Float, dy: Float)

    /**
     * A hardware key the console understands: 0..3 left/right/up/down, 4 return, 5 space,
     * 6 escape, 7 backspace, 8 page up, 9 page down, 10 tab, 11 Y, 12 X.
     */
    external fun nativeConsoleKey(handle: Long, key: Int, shift: Boolean, repeat: Boolean)

    /** Typed characters while the console reports `{"editing": true}`. */
    external fun nativeConsoleText(handle: Long, text: String)

    /**
     * Where the session the console asked for stands: 0 connecting, 1 streaming, 2 failed([message]),
     * 3 ended ([message] = the abnormal reason, or "" for a clean end), 4 reconnecting([message]).
     */
    external fun nativeConsoleSessionPhase(handle: Long, phase: Int, message: String)

    /** Re-root the console: `{}` = Home, `{"library": <HostRow>}` = that host's shelf over Home. */
    external fun nativeConsoleNavigate(handle: Long, entryJson: String)

    /**
     * The connected controllers: `{"label": "DualSense", "pref": 2, "pads": [{name, key, pref,
     * steam_virtual, battery: {percent, charging} | null}]}` — the chip and the settings rows.
     */
    external fun nativeConsoleSetPads(handle: Long, padsJson: String)

    /**
     * Block up to ~100 ms for the next event: `{"action": <OverlayAction>}`, `{"pulse": "move" |
     * "confirm" | "boundary"}`, `{"editing": bool}`, `{"announce": "<focused row>"}` (speak it),
     * `{"settings": <Settings>}` (persist it), `{"gles": 2 | 3}`, `{"dead": "<why>"}`. `""` on
     * timeout. Call from a dedicated poll thread.
     */
    external fun nativeConsoleNextEvent(handle: Long): String

    /** Every `ConsoleCmd` queued since the last call, as a JSON array (`[]` when none). */
    external fun nativeConsoleDrainCmds(handle: Long): String

    /** The home carousel's rows: `[HostRow]`. */
    external fun nativeConsoleSetHosts(handle: Long, json: String)

    /** The pairing ceremony's phase: `"Idle"`, `"Busy"`, `{"Failed": "why"}`, `{"Paired": {"key": …}}`. */
    external fun nativeConsoleSetPair(handle: Long, json: String)

    /** The wake card's status (`WakeStatus` JSON) or `null` to clear it. */
    external fun nativeConsoleSetWake(handle: Long, json: String)

    /**
     * A new phase for the speed test the console already raised: `"Connecting"`, `"Measuring"`,
     * `{"Failed": "why"}`, or `{"Done": {"throughput_kbps": …, "loss_pct": …,
     * "recommended_kbps": …}}`. There is no setter for the test itself — the shell owns that
     * slot, so a phase for a dismissed test, or for a host other than [key], is dropped.
     */
    external fun nativeConsoleAdvanceSpeed(handle: Long, key: String, json: String)

    /** A one-shot toast from a service worker. */
    external fun nativeConsoleNotice(handle: Long, text: String)

    /** `[{"heading", "text"}]`: what this app bundles, for the console's Licences screen. */
    external fun nativeConsoleSetLicenses(handle: Long, json: String)

    /** `{"held": [..], "axes": [[name, v]]}`: the pad's reading while the input test is on. */
    external fun nativeConsoleSetPadTest(handle: Long, json: String)

    /** A library fetch is starting for the shelf on screen (bumps the epoch, sets Loading). */
    external fun nativeConsoleLibraryBegin(handle: Long)

    /** `"Loading"`, `"Empty"`, `"Ready"`, or `{"Error": {"title", "body", "can_retry"}}`. */
    external fun nativeConsoleLibraryPhase(handle: Long, json: String)

    /** The catalog `[LibraryGame]`; [cached] = the last-known list shown while the fetch runs. */
    external fun nativeConsoleLibraryGames(handle: Long, json: String, cached: Boolean)

    /** One title's poster, encoded (JPEG/PNG bytes). */
    external fun nativeConsoleLibraryArt(handle: Long, id: String, bytes: ByteArray)

    /** The host's `/status` games: `[{"app_id": "steam:570", "state": "running"}, …]`. */
    external fun nativeConsoleLibraryRunning(handle: Long, json: String)

    /** 0 fresh, 1 waking, 2 offline — the cached shelf's staleness note. */
    external fun nativeConsoleLibraryStale(handle: Long, stale: Int)

    /** A settings change made elsewhere (touch UI, deep link): the shell reads it next. Not a save. */
    external fun nativeConsoleSetSettings(handle: Long, json: String)

    /** The preset catalog `[[id, name]]`. */
    external fun nativeConsoleSetPresets(handle: Long, json: String)

    /** The known-hosts records (`KnownHosts` JSON) the console builds `punktfunk://` links from. */
    external fun nativeConsoleSetKnownHosts(handle: Long, json: String)
}
