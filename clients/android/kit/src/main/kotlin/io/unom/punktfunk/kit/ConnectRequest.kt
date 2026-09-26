package io.unom.punktfunk.kit

import org.json.JSONObject

/**
 * Everything [NativeBridge.nativeConnect] needs, carried as one JSON object so the JNI seam has
 * one argument instead of twenty-six. The Rust side deserialises the same field names
 * (`session/connect.rs`, `ConnectRequest`), which is where each one is read.
 *
 * The identity is presented when both PEMs are non-empty, else the session is anonymous.
 * [pinHex] empty = trust-on-first-use (read [NativeBridge.nativeHostFingerprint] after); else the
 * 64-hex host SHA-256 to pin (mismatch → `0`). [timeoutMs] is the handshake budget — short for a
 * normal connect, long (≥ the host's approval-park window) for the no-PIN "request access" path.
 */
data class ConnectRequest(
    val host: String,
    val port: Int,
    /** The requested virtual-output mode; the host streams at exactly this. */
    val width: Int,
    val height: Int,
    val refreshHz: Int,
    val certPem: String,
    val keyPem: String,
    val pinHex: String,
    /** `0` = host default. */
    val bitrateKbps: Int,
    /** `CompositorPref` / `GamepadPref` wire bytes (`0` = Auto). */
    val compositorPref: Int,
    val gamepadPref: Int,
    val hdrEnabled: Boolean,
    /** Ask for 10-bit WITHOUT HDR — Main10 at BT.709, for banding-free gradients on an
     *  ordinary panel. Ignored while [hdrEnabled] is set, which already implies 10-bit. */
    val tenBitSdr: Boolean,
    /** Every decoder this device would use tolerates multi-slice AUs
     *  ([VideoDecoders.multiSliceTolerant]) — advertises `VIDEO_CAP_MULTI_SLICE`; false keeps
     *  the host at single-slice frames (the safe pre-0.17 wire shape). */
    val multiSliceOk: Boolean,
    /** Every decoder this device would use accepts partial-frame input
     *  ([VideoDecoders.partialFrameCapable]) — opts into slice-progressive delivery (the
     *  decode loop then feeds slices with `BUFFER_FLAG_PARTIAL_FRAME` as they arrive). */
    val framePartsOk: Boolean,
    val audioChannels: Int,
    /** Requested audio sample rate: **`0` (with [audioBits] `0`) for the legacy Opus plane**, or
     *  any rung of the lossless ladder — `44100`, `48000`, `88200`, `96000`, `176400`.
     *
     *  ⚠ **`48000`/`16` is NOT "the default" — it is the cheapest lossless rung.** Core sets
     *  `CLIENT_CAP_AUDIO_HIRES` when either field is non-zero, and the host's gate accepts
     *  48 kHz/16-bit, so passing it as a stand-in for "unset" opts every session into the `0xD3`
     *  plane. Send `0`/`0` for Standard.
     *
     *  A request on BOTH counts: the host's gate may answer Opus, and the native side first
     *  proves THIS device can open the rate (AAudio grants an explicit rate or fails the open,
     *  with no recovery once the wire is negotiated), downgrading the request if it cannot. */
    val audioRateHz: Int,
    /** Requested audio sample depth: `0` alongside a `0` [audioRateHz] for the legacy Opus
     *  plane, else `16` or `24`. */
    val audioBits: Int,
    /** `quic::CODEC_*` bitfield of codecs this device decodes ([VideoDecoders.decodableCodecBits]);
     *  `0` falls back to H.264|HEVC. The host resolves the emitted codec from this ∩ its GPU. */
    val videoCodecs: Int,
    /** Preferred video codec as a `quic::CODEC_*` bit (`0` = auto). Soft — the host falls back. */
    val preferredCodec: Int,
    val timeoutMs: Int,
    /** Store-qualified library id (`steam:<appid>` / `custom:<id>`) to boot straight into a game,
     *  or `null` for a plain desktop connect. Rides the Hello as `launch`. */
    val launch: String?,
    /** This device's display name (rides the Hello as `name`) — what the host's pending-approval
     *  list and trust store show for it. `null`/blank ⇒ the host's fingerprint-derived label. */
    val deviceName: String?,
    /** Advertise `CLIENT_CAP_PAD_AUDIO` — the SESSION-level negotiation for the 0xD1 per-pad
     *  DualSense plane. Without it the host never sets `HOST_CAP_PAD_AUDIO` and emits nothing. */
    val padAudioOk: Boolean,
    /** Advertise `CLIENT_CAP_KEEP_HOST_AUDIO` — the host taps its default playback device
     *  instead of parking it on a silent endpoint. REQUEST-only: an older host ignores it. */
    val keepHostAudio: Boolean,
    /** The `video_fit` setting (`"fit"`/`"crop"`/`"stretch"`). Rides the Hello, so a host that
     *  frames the picture for another device (a join, a mirrored head) reframes it for this one. */
    val videoFit: String = "fit",
    /** Build plus the shell and path that dialled (`"android 0.38.0 console/library"`). Rides the
     *  `Start` extension block, and is the host's `handshake complete` `client=` field: it is how
     *  two sessions from one device are told apart without a capture. */
    val dialer: String = "",
    /** The settings preset this session streams with, `null` for the plain settings. Rides the
     *  `Start` extension block; the host shows it and hands it to hooks and plugins. */
    val presetId: String? = null,
    /** That preset's name, for people. */
    val presetName: String? = null,
) {
    fun toJson(): String = JSONObject()
        .put("host", host)
        .put("port", port)
        .put("width", width)
        .put("height", height)
        .put("refresh_hz", refreshHz)
        .put("cert_pem", certPem)
        .put("key_pem", keyPem)
        .put("pin_hex", pinHex)
        .put("bitrate_kbps", bitrateKbps)
        .put("compositor_pref", compositorPref)
        .put("gamepad_pref", gamepadPref)
        .put("hdr_enabled", hdrEnabled)
        .put("ten_bit_sdr", tenBitSdr)
        .put("multi_slice_ok", multiSliceOk)
        .put("frame_parts_ok", framePartsOk)
        .put("audio_channels", audioChannels)
        .put("audio_rate_hz", audioRateHz)
        .put("audio_bits", audioBits)
        .put("video_codecs", videoCodecs)
        .put("preferred_codec", preferredCodec)
        .put("timeout_ms", timeoutMs)
        .put("launch", launch ?: JSONObject.NULL)
        .put("device_name", deviceName ?: JSONObject.NULL)
        .put("pad_audio_ok", padAudioOk)
        .put("keep_host_audio", keepHostAudio)
        .put("video_fit", videoFit)
        .put("dialer", dialer)
        .put("preset_id", presetId ?: JSONObject.NULL)
        .put("preset_name", presetName ?: JSONObject.NULL)
        .toString()
}
