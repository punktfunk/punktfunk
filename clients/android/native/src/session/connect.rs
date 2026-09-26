//! Connect lifecycle + the trust surface: identity mint, connect (TOFU / pinned), close,
//! host-fingerprint read, and the SPAKE2 PIN pairing ceremony.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::EnvUnowned;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{
    get_session, hex32, insert_session, jni_guard, lock_recover, parse_hex32, remove_session,
    SessionHandle,
};

/// Machine token of the most recent `nativeConnect`/`nativePair` failure, taken (and cleared)
/// by `nativeTakeLastError` so Kotlin can render a cause-specific message instead of the old
/// catch-all "wrong PIN, or the host isn't armed" (which blamed the PIN for dead network paths
/// — the moko0878-class support threads). The app runs one attempt at a time, so one slot
/// suffices; a stale token is harmless (it is taken immediately after the failed call).
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

/// Stable token for a failed pair/connect cause, matched by Kotlin (`ConnectErrors.kt`):
/// a typed host rejection yields its `RejectReason::as_str()` token ("not-armed", "denied",
/// "approval-timeout", …); transport-level causes map to "crypto" / "timeout" / "io" / "error".
fn note_error(e: &punktfunk_core::error::PunktfunkError) {
    use punktfunk_core::error::PunktfunkError as E;
    let token = match e {
        E::Rejected(r) => r.as_str(),
        E::Crypto => "crypto",
        E::Timeout => "timeout",
        E::Io(_) => "io",
        _ => "error",
    };
    *lock_recover(&LAST_ERROR) = token.to_string();
}

/// `NativeBridge.nativeTakeLastError(): String` — the machine token of the most recent failed
/// `nativeConnect`/`nativePair`, cleared on read (`""` when none). Call right after a `0`
/// handle / `""` fingerprint.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeTakeLastError<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> JString<'local> {
    let token = std::mem::take(&mut *lock_recover(&LAST_ERROR));
    env.with_env(|env| env.new_string(token))
        .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeGenerateIdentity(): String` — mint a fresh persistent self-signed identity.
/// Returns `"<certPem>\n-----PUNKTFUNK-KEY-----\n<keyPem>"`, or `""` on failure (logged). Kotlin
/// persists it (Keystore-wrapped) and only calls this again when the store is genuinely empty.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeGenerateIdentity<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> JString<'local> {
    let out = match punktfunk_core::quic::endpoint::generate_identity() {
        Ok((cert, key)) => format!("{cert}\n-----PUNKTFUNK-KEY-----\n{key}"),
        Err(e) => {
            log::error!("nativeGenerateIdentity failed: {e}");
            String::new()
        }
    };
    env.with_env(|env| env.new_string(out))
        .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSetLowLatencyMode(enabled)` — apply the user's "Low-latency mode
/// (experimental)" toggle to the process-wide transport defaults, today just DSCP/QoS marking on
/// the media sockets. Must be called BEFORE `nativeConnect` (the tag is applied at socket
/// creation); Kotlin's one connect choke point (`HostConnect.connectToHost`) does. The rest of the
/// toggle rides explicit per-session parameters (`nativeStartVideo` / `nativeStartAudio`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetLowLatencyMode(
    _env: EnvUnowned,
    _this: JObject,
    enabled: jboolean,
) {
    jni_guard((), || {
        punktfunk_core::transport::set_dscp_default(enabled);
    })
}

/// `debug.punktfunk.force_parts` = 1: arm slice-progressive parts delivery even when the
/// Kotlin `FEATURE_PartialFrame` probe said no — the rebuild-free on-glass experiment for a
/// decoder that may accept `BUFFER_FLAG_PARTIAL_FRAME` without declaring the feature (the NP3's
/// c2.qti decoders declare nothing). Android-only; everywhere else the probe verdict stands.
#[cfg(target_os = "android")]
fn force_parts_sysprop() -> bool {
    let mut buf = [0u8; 92]; // PROP_VALUE_MAX
                             // SAFETY: __system_property_get with a valid name + PROP_VALUE_MAX buffer is always safe.
    let n = unsafe {
        libc::__system_property_get(
            c"debug.punktfunk.force_parts".as_ptr(),
            buf.as_mut_ptr().cast(),
        )
    };
    n > 0 && std::str::from_utf8(&buf[..n as usize]).unwrap_or("").trim() == "1"
}

#[cfg(not(target_os = "android"))]
fn force_parts_sysprop() -> bool {
    false
}

/// The rates this session may ask for when the one the user chose will not open, best first.
///
/// **Down the requested rate's own FAMILY, then the 48 kHz floor.** The two families
/// ([`punktfunk_core::audio::pcm::rate_is_supported`]) are 44.1 / 88.2 / 176.4 kHz and 48 / 96 kHz,
/// and within a family the lower rates are the same material at half the samples — a 176.4 kHz
/// interface that will not open is overwhelmingly likely to be an 88.2 or 44.1 kHz one, and asking
/// there next is asking for the rate the endpoint most plausibly runs at.
///
/// **48 kHz terminates every ladder, including the 44.1 family's**, and that crossing is deliberate
/// rather than an oversight. It is the rate every Android output grants, it is the rate this
/// protocol has always run, and the alternative to a 48 kHz *lossless* session is a 48 kHz *Opus*
/// one — the same rate with a lossy stage added. Nothing is resampled by this decision: the host
/// captures at the rate it answers with, or declines (§8.2/§8.3), so a 44.1 kHz-locked endpoint
/// answers a 48 kHz request with Opus rather than with a quiet upsample.
///
/// The requested rate is the first rung, so an openable rate is asked for unchanged and a default
/// session's ladder is one rung long.
fn rate_fallback_ladder(rate_hz: u32) -> &'static [u32] {
    const HZ48: u32 = punktfunk_core::audio::SAMPLE_RATE_HZ;
    match rate_hz {
        176_400 => &[176_400, 88_200, 44_100, HZ48],
        88_200 => &[88_200, 44_100, HZ48],
        44_100 => &[44_100, HZ48],
        96_000 => &[96_000, HZ48],
        // 48 kHz itself, and — via the `rate_is_supported` guard in the caller — nothing else.
        _ => &[HZ48],
    }
}

/// Resolve the audio format this `Hello` should ASK for, from what Kotlin's setting requested —
/// after proving this device can actually open it.
///
/// This is `design/hi-res-audio.md` §7's rule made mechanical: *"a client that cannot open a
/// 96 kHz output must not set `CLIENT_CAP_AUDIO_HIRES`"*. It has to happen here, before the
/// handshake, because after it there is no recovery: AAudio grants an explicitly-requested rate or
/// fails the open (it never substitutes), the host does not renegotiate the plane mid-session
/// (§6), and the only ways to play a wire of one rate through a stream of another are the wrong
/// speed or a resampler nobody asked for — which §9 forbids in as many words ("say so and fall
/// back, not resample quietly"). So the fall back happens where falling back is still free: in the
/// request.
///
/// **Every rung above the floor is probed, and admitting the 44.1 kHz family made that matter
/// more, not less.** When the ladder was 96 → 48 there was one uncertain rate; now there are four,
/// and their odds are nothing alike — 44 100 Hz is granted by very nearly every Android output,
/// 176 400 Hz by very nearly none, and 88 200 Hz by whatever the HAL happens to think. None of
/// that is inferable from the number, so [`crate::audio::output_rate_is_openable`] opens a stream
/// and reads back what it was granted, once per rung, until one holds.
///
/// Dropping the RATE keeps the depth, so a device that refuses the rate still gets a 24-bit
/// lossless session rather than being pushed all the way back to Opus — the depth is where the
/// plane earns its bandwidth anyway (and it is the half that is audible at all: §12).
///
/// The 48 kHz floor is never probed. It is universally supported, and the DEPTH never reaches
/// AAudio at all (the device is opened as f32 on both planes — see `crate::audio`), so there is
/// nothing about 16-vs-24-bit for a probe to discover. An ordinary session therefore opens no
/// stream here and pays nothing.
///
/// # ⚠⚠ "Not asking" is `(0, 0)`, and it is NOT `(48 000, 16)`
///
/// Core's `advertised_client_caps` sets `CLIENT_CAP_AUDIO_HIRES` when **either field is non-zero**
/// — it keys on *the caller specified a format*, not on *the format differs from the default*, and
/// deliberately: 48 kHz/16-bit is the cheapest lossless rung as well as the legacy pair, so a
/// "differs from the default" rule would make it the one rung on the ladder nobody could ask for.
///
/// So returning the legacy-looking `(48 000, 16)` for a user who chose **Standard (Opus)** does not
/// mean "no request" — it advertises the capability, the host's gate accepts 48 kHz/16-bit as a
/// perfectly supported format, and the host then silently gives that user the lossless `0xD3` plane
/// at 1.5 Mbps in place of 256 kbps of Opus. This returned exactly that pair until it was caught by
/// comparing all four clients; the desktop client and every pre-v24 `punktfunk_connect_ex*` send
/// `(0, 0)`, and so does this now.
///
/// ⚠⚠ The reach of that slip grew on 2026-08-17: `PUNKTFUNK_AUDIO_HIRES` went default-ON, so where
/// this used to need a host whose operator had opted in, it now lands on every host that has not
/// deliberately opted out.
///
/// `(0, 0)` is also what keeps the `Hello` byte-identical to a legacy one, because the wire encodes
/// an explicit 48 000/16 the same as absent — the difference lives entirely in the capability bit.
fn resolve_requested_audio_format(rate_hz: u32, bits: u8, channels: u8) -> (u32, u8) {
    const HZ48: u32 = punktfunk_core::audio::SAMPLE_RATE_HZ;
    /// "This session did not ask for the lossless plane" — see the ⚠⚠ section above for why this
    /// is a pair of zeroes and not the legacy 48 000/16.
    const UNSPECIFIED: (u32, u8) = (0, 0);
    // A format core would not carry — including Kotlin's `0`/`0` for the Opus setting — asks for
    // nothing, rather than being an error: the request is a preference, and an unrecognized one
    // must not block a connect. The rate set comes from core rather than being re-expressed here,
    // so the host's gate and every client's request validation cannot drift apart.
    if !punktfunk_core::audio::pcm::depth_is_supported(bits)
        || !punktfunk_core::audio::pcm::rate_is_supported(rate_hz)
    {
        return UNSPECIFIED;
    }
    let granted = rate_fallback_ladder(rate_hz)
        .iter()
        .copied()
        // `HZ48` short-circuits the probe rather than being trusted after one: it is the ladder's
        // floor, so a probe there could only turn a working session into no lossless session at
        // all — and it is the rate a failed probe would have fallen back TO.
        .find(|&hz| hz == HZ48 || audio_rate_is_openable(hz, channels))
        // Unreachable while every ladder ends at `HZ48`; the belt is here so a future rung added
        // above the floor cannot silently produce an unrequestable format.
        .unwrap_or(HZ48);
    if granted != rate_hz {
        log::warn!(
            "audio: this device will not open a {rate_hz} Hz output, so the session asks for {granted} Hz / {bits}-bit instead — the wire is only ever offered a format this client has proved it can play"
        );
    }
    (granted, bits)
}

#[cfg(target_os = "android")]
fn audio_rate_is_openable(rate_hz: u32, channels: u8) -> bool {
    crate::audio::output_rate_is_openable(rate_hz, channels)
}

/// Off-device (the host `cargo build --workspace` leg, where there is no AAudio at all): nothing
/// can be proved, so nothing is claimed. The caller falls back to the legacy rate, which is the
/// safe answer for a build that never runs on a phone anyway.
#[cfg(not(target_os = "android"))]
fn audio_rate_is_openable(_rate_hz: u32, _channels: u8) -> bool {
    false
}

/// The `Hello`'s video capability bits, from what the panel can present and what the decoders
/// tolerate.
///
/// `hdr` is panel truth: Kotlin checks `Display.getHdrCapabilities()`, because asking for PQ on
/// an SDR screen gets a stream the panel mis-tone-maps. It implies 10 bits, so it carries both.
///
/// `ten_bit_sdr` is the separate ask, and not an HDR question: Main10 at BT.709 costs a little
/// bandwidth and buys gradients that do not band, on any panel. The decode path needs nothing for
/// it — the HDR metadata and the Surface dataspace are gated on the stream's own colour, so an
/// SDR Main10 stream skips both.
///
/// `multi_slice` is decoder truth: Kotlin probes every decoder this device would use because
/// Amlogic can wedge on multi-slice AUs. Only then may the host send more than one slice.
///
/// Android ARMv7 always asks for ChaCha because software AES limits these TV-class devices. Other
/// ABIs keep AES; their hardware acceleration makes the tradeoff target-specific.
fn video_caps(hdr: bool, ten_bit_sdr: bool, multi_slice: bool) -> u8 {
    use punktfunk_core::quic::{
        VIDEO_CAP_10BIT, VIDEO_CAP_CHACHA20, VIDEO_CAP_HDR, VIDEO_CAP_MULTI_SLICE,
    };
    let mut caps = if cfg!(all(target_os = "android", target_arch = "arm")) {
        VIDEO_CAP_CHACHA20
    } else {
        0
    };
    if hdr {
        caps |= VIDEO_CAP_10BIT | VIDEO_CAP_HDR;
    }
    if ten_bit_sdr {
        caps |= VIDEO_CAP_10BIT;
    }
    if multi_slice {
        caps |= VIDEO_CAP_MULTI_SLICE;
    }
    caps
}

#[cfg(test)]
mod caps_tests {
    use super::video_caps;
    use punktfunk_core::quic::{
        VIDEO_CAP_10BIT, VIDEO_CAP_CHACHA20, VIDEO_CAP_HDR, VIDEO_CAP_MULTI_SLICE,
    };

    fn automatic_caps() -> u8 {
        video_caps(false, false, false)
    }

    /// HDR carries 10-bit with it, 10-bit-SDR asks for the depth alone, and neither reaches for
    /// the other's bit. The whole point of the split: an SDR panel can still get Main10.
    #[test]
    fn ten_bit_is_asked_for_with_or_without_hdr() {
        let automatic = automatic_caps();
        assert_eq!(
            video_caps(true, false, false),
            automatic | VIDEO_CAP_10BIT | VIDEO_CAP_HDR
        );
        assert_eq!(video_caps(false, true, false), automatic | VIDEO_CAP_10BIT);
        // HDR already implies the depth, so asking for both is the same request as HDR.
        assert_eq!(
            video_caps(true, true, false),
            automatic | VIDEO_CAP_10BIT | VIDEO_CAP_HDR
        );
        // …and 10-bit alone never implies PQ, which would be the mis-tone-mapped stream.
        assert_eq!(video_caps(false, true, false) & VIDEO_CAP_HDR, 0);
    }

    /// Multi-slice is decoder truth and rides alongside, never gated by the colour asks.
    #[test]
    fn multi_slice_is_independent_of_the_colour_bits() {
        let automatic = automatic_caps();
        assert_eq!(
            video_caps(false, false, true),
            automatic | VIDEO_CAP_MULTI_SLICE
        );
        assert_eq!(
            video_caps(true, false, true),
            automatic | VIDEO_CAP_10BIT | VIDEO_CAP_HDR | VIDEO_CAP_MULTI_SLICE
        );
    }

    #[cfg(all(target_os = "android", target_arch = "arm"))]
    #[test]
    fn armv7_android_automatically_requests_chacha() {
        assert_eq!(automatic_caps() & VIDEO_CAP_CHACHA20, VIDEO_CAP_CHACHA20);
    }

    #[cfg(not(all(target_os = "android", target_arch = "arm")))]
    #[test]
    fn other_targets_keep_aes() {
        assert_eq!(automatic_caps() & VIDEO_CAP_CHACHA20, 0);
    }
}

/// What Kotlin hands `nativeConnect`, as `ConnectRequest.toJson()` writes it (kit/ConnectRequest.kt
/// carries the per-field contract). Every field is a request the host may answer differently;
/// `connector.*` after the handshake is what actually happened.
#[derive(serde::Deserialize)]
struct ConnectRequest {
    host: String,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    /// Both non-empty ⇒ presented as the persistent identity, else anonymous.
    #[serde(default)]
    cert_pem: String,
    #[serde(default)]
    key_pem: String,
    /// Empty ⇒ TOFU; else 64-hex SHA-256 of the host cert (mismatch ⇒ 0).
    #[serde(default)]
    pin_hex: String,
    /// 0 = host default.
    #[serde(default)]
    bitrate_kbps: u32,
    /// `CompositorPref` / `GamepadPref` wire bytes (unknown ⇒ Auto).
    #[serde(default)]
    compositor_pref: u8,
    #[serde(default)]
    gamepad_pref: u8,
    #[serde(default)]
    hdr_enabled: bool,
    #[serde(default)]
    ten_bit_sdr: bool,
    #[serde(default)]
    multi_slice_ok: bool,
    #[serde(default)]
    frame_parts_ok: bool,
    /// Requested surround layout (2/6/8; anything else ⇒ stereo).
    #[serde(default)]
    audio_channels: u8,
    /// The audio FORMAT asked for. `0`/`0` is "did not ask" (the Opus plane); any pair core can
    /// carry asks for the lossless `0xD3` plane, `48000`/`16` INCLUDED — that is the cheapest
    /// lossless rung, not a spelling of "default". [`resolve_requested_audio_format`] downgrades
    /// it here, before the `Hello`, if this device cannot open the rate.
    #[serde(default)]
    audio_rate_hz: u32,
    #[serde(default)]
    audio_bits: u8,
    /// `CODEC_*` bits this device decodes; 0 falls back to H.264|HEVC.
    #[serde(default)]
    video_codecs: u8,
    /// Soft codec preference wire byte (0 = Auto).
    #[serde(default)]
    preferred_codec: u8,
    /// Handshake budget: short for a normal connect, long (≥ the host's approval-park window) for
    /// the no-PIN "request access" path so a slow operator approval lands on this connection.
    timeout_ms: u64,
    /// A store-qualified library id to boot straight into a game; rides the Hello as `launch`.
    #[serde(default)]
    launch: Option<String>,
    /// The host's approval-list / trust-store label for this device; rides the Hello as `name`.
    #[serde(default)]
    device_name: Option<String>,
    #[serde(default)]
    pad_audio_ok: bool,
    #[serde(default)]
    keep_host_audio: bool,
    /// The `video_fit` setting (`"fit"`/`"crop"`/`"stretch"`); absent reads as fit.
    #[serde(default)]
    video_fit: String,
    /// Build plus the shell that dialled; rides `Start`'s extension block as the host's log label.
    #[serde(default)]
    dialer: String,
    /// The settings preset this session streams with; `None` for the plain settings.
    #[serde(default)]
    preset_id: Option<String>,
    /// That preset's name.
    #[serde(default)]
    preset_name: Option<String>,
}

/// `NativeBridge.nativeConnect(requestJson): Long` — see [`ConnectRequest`]. Returns an opaque
/// handle, or 0 on failure (`nativeTakeLastError` carries the cause).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConnect<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    request: JString<'local>,
) -> jlong {
    // The one `Env` scope jni 0.22 grants a native method reads the JSON; everything below is
    // pure Rust over the owned request.
    let req: Option<ConnectRequest> = env
        .with_env(|env| -> jni::errors::Result<Option<ConnectRequest>> {
            let text = request.try_to_string(env)?;
            Ok(match serde_json::from_str::<ConnectRequest>(&text) {
                Ok(r) => Some(r),
                Err(e) => {
                    log::error!("nativeConnect: bad request JSON from Kotlin: {e}");
                    None
                }
            })
        })
        .resolve::<LogErrorAndDefault>();
    let Some(req) = req else {
        return 0;
    };
    jni_guard(0, || connect(req))
}

/// The connect proper, off the JNI seam.
fn connect(req: ConnectRequest) -> jlong {
    let ConnectRequest {
        host,
        port,
        width,
        height,
        refresh_hz,
        cert_pem: cert,
        key_pem: key,
        pin_hex,
        bitrate_kbps,
        compositor_pref,
        gamepad_pref,
        hdr_enabled,
        ten_bit_sdr,
        multi_slice_ok,
        frame_parts_ok,
        audio_channels,
        audio_rate_hz,
        audio_bits,
        video_codecs,
        preferred_codec,
        timeout_ms,
        launch,
        device_name,
        pad_audio_ok,
        keep_host_audio,
        video_fit,
        dialer,
        preset_id,
        preset_name,
    } = req;
    // Which shell asked, for the host's `handshake complete` line. Set before the dial: core reads
    // it once the host says it parses the block.
    punktfunk_core::client::set_client_label(&dialer);
    // Per dial, like the label: a session without a preset must not name the last one's.
    punktfunk_core::client::set_session_preset(preset_id.as_deref().and_then(|id| {
        punktfunk_core::quic::SessionPreset::new(id, preset_name.as_deref().unwrap_or(""))
    }));
    let launch = launch.filter(|s| !s.is_empty());
    let device_name = device_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let identity: Option<(String, String)> = if cert.is_empty() || key.is_empty() {
        None
    } else {
        Some((cert, key))
    };
    // Slice-progressive parts, by decoder truth (Kotlin's FEATURE_PartialFrame probe) — with a
    // sysprop escape hatch for the on-glass science question the probe can't answer: does the
    // decoder ACTUALLY choke on BUFFER_FLAG_PARTIAL_FRAME input, or does it merely not declare
    // the feature? (`adb shell setprop debug.punktfunk.force_parts 1` + stream restart; a codec
    // that can't take parts errors recoverably and the reanchor gate + keyframe path recovers.)
    let force_parts = force_parts_sysprop();
    let frame_parts = frame_parts_ok || force_parts;
    // The connect-time capability readout (`adb logcat -s pf.caps`): the P2 slice pipeline is
    // inert client-side unless BOTH probes pass — this line is the one place that says which.
    log::info!(
        target: "pf.caps",
        "decoder caps: multi_slice={} partial_frame={}{} hdr={} ten_bit_sdr={} codec_bits={:#x}",
        multi_slice_ok,
        frame_parts_ok,
        if force_parts { " (FORCED by sysprop)" } else { "" },
        hdr_enabled,
        ten_bit_sdr,
        video_codecs,
    );
    let pin: Option<[u8; 32]> = if pin_hex.is_empty() {
        None
    } else {
        match parse_hex32(&pin_hex) {
            Some(fp) => Some(fp),
            None => {
                log::error!("nativeConnect: bad pin hex (len {})", pin_hex.len());
                return 0;
            }
        }
    };
    let mode = Mode {
        width,
        height,
        refresh_hz,
    };
    // Requested surround layout (2 = stereo / 6 = 5.1 / 8 = 7.1); anything else is stereo. The
    // host clamps it and echoes the resolved count in `connector.audio_channels`, which drives the
    // decoder + AAudio layout (read in `crate::audio::AudioPlayback::start`).
    let audio_channels = punktfunk_core::audio::normalize_channels(audio_channels);
    // The audio format, downgraded to something this device has PROVED it can open before the
    // `Hello` carries it — see `resolve_requested_audio_format` for why it cannot wait until
    // playback.
    let (audio_rate_hz, audio_bits) =
        resolve_requested_audio_format(audio_rate_hz, audio_bits, audio_channels);
    match NativeClient::connect_with_audio_format(
        &host,
        port,
        mode,
        CompositorPref::from_u8(compositor_pref),
        GamepadPref::from_u8(gamepad_pref),
        bitrate_kbps, // 0 = host default
        video_caps(hdr_enabled, ten_bit_sdr, multi_slice_ok),
        audio_channels,
        // The audio format this session ASKS for (resolved above). A non-default pair is what
        // makes core set `CLIENT_CAP_AUDIO_HIRES` in the `Hello` — capable AND the user turned it
        // on, the `VIDEO_CAP_444` precedent — and it is answered by the host re-formatting the
        // wire, so it must never be advertised on a device that cannot open the output. The host
        // may still decline; `connector.audio_codec`/`audio_sample_rate_hz`/`audio_bits` are what
        // actually happened, and `crate::audio` opens the device from those, never from these.
        audio_rate_hz,
        audio_bits,
        // Legacy coupling: libopus here decodes either, and nothing on Android needs the other.
        punktfunk_core::audio::AudioLayout::Legacy,
        punktfunk_core::video_fit::VideoFit::from_name(&video_fit),
        // Codecs this device decodes (`VideoDecoders.decodableCodecBits`): H.264 + HEVC always,
        // AV1 on a real `video/av01` decoder, PyroWave on a GPU that passes the probe — the one
        // bit here naming no MediaCodec, since it decodes as Vulkan compute in `crate::pyro`.
        // Masked to the known bits, falling back to H.264|HEVC on 0 so a bogus value cannot
        // advertise nothing and kill the handshake. The host echoes its pick in `connector.codec`.
        {
            let bits = video_codecs
                & (punktfunk_core::quic::CODEC_H264
                    | punktfunk_core::quic::CODEC_HEVC
                    | punktfunk_core::quic::CODEC_AV1
                    | punktfunk_core::quic::CODEC_PYROWAVE);
            if bits == 0 {
                punktfunk_core::quic::CODEC_H264 | punktfunk_core::quic::CODEC_HEVC
            } else {
                bits
            }
        },
        preferred_codec,
        // No display-volume forwarding from Android yet (the panel tone-maps PQ itself via the
        // Surface dataspace + static metadata) — the host keeps its virtual-display EDID defaults.
        None,
        // No CLIENT_CAP_CURSOR: this client does not render the host cursor locally (no
        // shape/state planes in the jni surface) — advertising it would stream cursor-less.
        // CLIENT_CAP_PHASE_LOCK is honest: the async decode loop's presenter feeds
        // report_phase (advisory in v1 — the host arms on report receipt — but the Hello
        // should say what the client does).
        // CLIENT_CAP_PAD_AUDIO is the SESSION-level negotiation, separate from the per-pad
        // arrival bits: without it the host never sets HOST_CAP_PAD_AUDIO and never emits 0xD1,
        // so declaring a pad's render caps later would have nothing to gate. Gated on the
        // settings so a user with pad audio off does not make the host provision endpoints.
        punktfunk_core::quic::CLIENT_CAP_PHASE_LOCK
            | if pad_audio_ok {
                punktfunk_core::quic::CLIENT_CAP_PAD_AUDIO
            } else {
                0
            }
            // The user's "Keep host audio playing" setting: the host taps whatever its default
            // playback device already is instead of parking the desktop mix on a silent
            // endpoint, so the speakers on the host PC stay live. REQUEST-only — there is no
            // host-cap echo — so an older host ignores the bit and re-routes exactly as it
            // always did ("the host went quiet"), never a broken session.
            | if keep_host_audio {
                punktfunk_core::quic::CLIENT_CAP_KEEP_HOST_AUDIO
            } else {
                0
            },
        // Slice-progressive delivery, by decoder truth (Kotlin probes FEATURE_PartialFrame on
        // every decoder this device would use; `debug.punktfunk.force_parts` overrides for the
        // on-glass experiment): AU prefixes then arrive as `Frame::part` pieces and the decode
        // loop feeds them with BUFFER_FLAG_PARTIAL_FRAME.
        frame_parts,
        launch, // a store-qualified library id to boot into a game, or None for the desktop
        device_name, // Kotlin's Build.MODEL — the host's approval-list / trust-store label
        pin,    // Some → Crypto on host-fp mismatch
        identity, // owned (cert, key) PEM, or None (anonymous)
        // Handshake budget from Kotlin: ~10 s for a normal connect, ~185 s for "request access"
        // (the host parks the connection until the operator approves the device — see ConnectScreen).
        Duration::from_millis(timeout_ms),
        // The Kotlin side cancels by dropping the result (`Dial.cancelled`), not by aborting
        // the dial — its connect runs on a pool thread, so a parked one costs a thread, not a
        // stuck UI. Wire a flag through here if that ever stops being true.
        None,
    ) {
        Ok(client) => {
            let client = Arc::new(client);
            let handle = SessionHandle {
                stats: Arc::new(crate::stats::VideoStats::new(client.hud_shared())),
                client,
                video: Mutex::new(None),
                drain: Mutex::new(None),
                #[cfg(target_os = "android")]
                audio: Mutex::new(None),
                #[cfg(target_os = "android")]
                mic: Mutex::new(None),
                #[cfg(target_os = "android")]
                pad_audio: Mutex::new(None),
                // A fresh session is never muted (mute is per-session UI state, not a setting).
                mic_muted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                access_seq: std::sync::atomic::AtomicU32::new(0),
                // Reported by Kotlin at `surfaceCreated` and on every resize after it.
                surface_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                // The full frame until Kotlin places the picture.
                src_crop: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                decoded_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            };
            insert_session(handle)
        }
        Err(e) => {
            log::error!("nativeConnect to {host}:{port} failed: {e}");
            note_error(&e);
            0
        }
    }
}

/// `NativeBridge.nativeClose(handle)` — remove one session key and begin teardown.
///
/// Existing JNI calls retain their `Arc` until they return, then the final drop joins media workers
/// and closes the connector. Zero, stale, duplicate, and concurrent closes are no-ops.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClose(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || drop(remove_session(handle)))
}

/// Mark an explicit user disconnect so the host skips reconnect linger.
///
/// Missing keys are ignored. The table lookup retains the session through this call even when
/// `nativeClose` removes the key concurrently.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeDisconnectQuit(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        if let Some(session) = get_session(handle) {
            session.client.disconnect_quit();
        }
    })
}

/// Request an in-stream resolution switch without reconnecting.
///
/// Returns `false` for a missing session, nonpositive geometry, or a closed request channel.
/// A concurrent close cannot invalidate the table-retained session during this enqueue.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeRequestMode(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    width: jint,
    height: jint,
    refresh_hz: jint,
) -> jboolean {
    jni_guard(false, || {
        if width <= 0 || height <= 0 || refresh_hz <= 0 {
            return false;
        }
        let Some(session) = get_session(handle) else {
            return false;
        };
        session
            .client
            .request_mode(Mode {
                width: width as u32,
                height: height as u32,
                refresh_hz: refresh_hz as u32,
            })
            .is_ok()
    })
}

/// Return the connected host certificate's SHA-256 as 64 lowercase hex characters.
/// Missing or concurrently closed session keys return an empty string.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeHostFingerprint<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    let out = get_session(handle)
        .map(|session| hex32(&session.client.host_fingerprint))
        .unwrap_or_default();
    env.with_env(|env| env.new_string(out))
        .resolve::<LogErrorAndDefault>()
}

/// Report whether the keyed QUIC session has ended.
///
/// The stream watchdog polls this to leave a frozen final frame after disconnect. Missing or
/// concurrently closed keys return `false`; a successful lookup is one atomic client read.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSessionEnded(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    jni_guard(false, || {
        get_session(handle).is_some_and(|session| session.client.is_session_ended())
    })
}

/// Return the session's `PunktfunkEndReason` byte for Kotlin's disconnect message.
///
/// `0` means no reason, including a missing or concurrently closed key. A retained lookup
/// distinguishes deliberate host closure from network loss without exposing session memory.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeEndReason(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jint {
    jni_guard(0, || {
        get_session(handle)
            .map(|session| session.client.end_reason() as jint)
            .unwrap_or(0)
    })
}

/// `NativeBridge.nativePair(host, port, certPem, keyPem, pin, name): String` — run the SPAKE2 PIN
/// ceremony, presenting our persistent identity. On success returns the host's verified fingerprint
/// (64-hex) to persist + pin; on any failure (wrong PIN / MITM / host reject / unreachable) returns
/// `""` (logged). Blocking — Kotlin calls it off the UI thread.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativePair<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    host: JString<'local>,
    port: jint,
    cert_pem: JString<'local>,
    key_pem: JString<'local>,
    pin: JString<'local>,
    name: JString<'local>,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let g = |e: &jni::Env<'local>, j: &JString<'local>| -> String {
            j.try_to_string(e).unwrap_or_default()
        };
        let host = g(env, &host);
        let cert = g(env, &cert_pem);
        let key = g(env, &key_pem);
        let pin = g(env, &pin);
        let name = g(env, &name);

        let out = if host.is_empty() || cert.is_empty() || key.is_empty() {
            log::error!("nativePair: missing host/identity");
            String::new()
        } else {
            match NativeClient::pair(
                &host,
                port as u16,
                (&cert, &key), // borrowed identity
                &pin,
                &name,
                Duration::from_secs(60),
            ) {
                Ok(host_fp) => hex32(&host_fp),
                Err(e) => {
                    // Crypto error == wrong PIN / MITM; anything else == transport/host reject.
                    // The token lets Kotlin say WHICH (`nativeTakeLastError`).
                    log::error!("nativePair to {host}:{port} failed: {e}");
                    note_error(&e);
                    String::new()
                }
            }
        };
        env.new_string(out)
    })
    .resolve::<LogErrorAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::audio::pcm::{BITS_16, BITS_24};
    use punktfunk_core::audio::SAMPLE_RATE_HZ;

    /// The rule this leg exists to enforce: the `Hello` never asks for an audio format this device
    /// has not proved it can open, because after the handshake there is no way back — AAudio grants
    /// an explicit rate or fails the open, the host does not renegotiate the plane mid-session, and
    /// playing a 96 kHz wire through a 48 kHz stream is not a fallback, it is the wrong audio.
    ///
    /// Off-device (this test's target) `audio_rate_is_openable` answers `false` for everything, so
    /// what is pinned here is the DOWNGRADE, which is the half that has to be right: a device that
    /// cannot do the rate still gets a lossless session at 48 kHz rather than being pushed all the
    /// way back to Opus, and the depth — the thing lossless is actually for — survives.
    #[test]
    fn an_unopenable_rate_is_downgraded_before_the_hello_and_keeps_its_depth() {
        // 48 kHz/16-bit is the cheapest LOSSLESS rung, not a way of spelling "default" — asking
        // for it explicitly passes through and probes nothing. What a default session sends is
        // `(0, 0)`, pinned in `an_opus_session_asks_for_nothing_and_a_lossless_one_asks_for_
        // something`, and conflating the two is what silently upgraded every Opus user.
        assert_eq!(
            resolve_requested_audio_format(SAMPLE_RATE_HZ, BITS_16, 2),
            (SAMPLE_RATE_HZ, BITS_16)
        );
        // 48 kHz is never probed, so 48/24 lossless survives even where nothing can be opened.
        assert_eq!(
            resolve_requested_audio_format(SAMPLE_RATE_HZ, BITS_24, 2),
            (SAMPLE_RATE_HZ, BITS_24)
        );
        // Every rung above the floor IS probed, is refused here, and lands on 48 kHz with the
        // depth intact — including the whole 44.1 kHz family, which this pass admitted. AAudio
        // never substitutes a rate, so a device that would not grant 176 400 Hz and was asked for
        // it anyway is silence, not a slower session.
        for rate in [44_100u32, 88_200, 96_000, 176_400] {
            assert_eq!(
                resolve_requested_audio_format(rate, BITS_24, 2),
                (SAMPLE_RATE_HZ, BITS_24),
                "{rate} Hz should have fallen to 48 kHz and kept 24-bit"
            );
        }
        // Surround asks exactly as stereo does. The lossless plane was stereo-only while a
        // surround frame did not fit a datagram; the frame ladder is channel-aware, the host
        // decides, and this leg's only job is to prove the OUTPUT opens (at the layout it will
        // actually be opened with).
        assert_eq!(
            resolve_requested_audio_format(SAMPLE_RATE_HZ, BITS_24, 6),
            (SAMPLE_RATE_HZ, BITS_24)
        );
    }

    /// A settings string, a preset written by a newer build, or a corrupted preference must never
    /// reach the wire as a format the plane cannot carry — and must never block a connect either.
    /// Every one resolves to the "did not ask" sentinel, which every host can answer.
    ///
    /// The rate set is `pcm::rate_is_supported`'s, not a second copy of it: 44 100 Hz used to sit
    /// in this table because §4.1's integer samples-per-millisecond arithmetic could not express
    /// it, and the day that stopped being true a locally re-expressed set would have kept refusing
    /// it with a stale reason.
    #[test]
    fn an_unrepresentable_request_falls_back_instead_of_failing() {
        for (rate, bits) in [
            (0, 0),               // Kotlin's Opus setting, and its "unset"
            (22_050, BITS_24),    // below the ladder — a rate this protocol never negotiates
            (192_000, BITS_24),   // out by §3's scope decision, not by any arithmetic
            (384_000, BITS_24),   // above anything anyone has asked for
            (SAMPLE_RATE_HZ, 32), // 32-bit float is deliberately not on the wire
            (SAMPLE_RATE_HZ, 8),  // not a depth this plane carries
            (176_400, 32),        // a carried rate cannot rescue an uncarried depth
        ] {
            assert_eq!(
                resolve_requested_audio_format(rate, bits, 2),
                (0, 0),
                "{rate} Hz / {bits}-bit should have asked for nothing at all"
            );
        }
    }

    /// ⚠⚠ **The one that decides whether a user who chose Opus is quietly given 1.5 Mbps of PCM.**
    ///
    /// Core's `advertised_client_caps` sets `CLIENT_CAP_AUDIO_HIRES` when **either** field of the
    /// pair below is non-zero. It keys on "a format was specified" rather than "the format differs
    /// from the default", and deliberately: 48 kHz/16-bit is the legacy pair AND the cheapest
    /// lossless rung, so the other rule would make that rung the one nobody could request.
    ///
    /// The consequence is that `(48 000, 16)` is not a way of saying "default" — it is a request,
    /// the host's gate accepts it as a supported format, and the user who chose Standard gets the
    /// lossless `0xD3` plane instead of Opus on every host that has not opted out of it (which,
    /// since the host gate went default-ON on 2026-08-17, is all of them). Nothing surfaces it: the
    /// settings screen shows what was asked for, and a granted
    /// session and a declined one look identical from there. This function returned that pair until
    /// all four clients were compared against each other.
    ///
    /// The rule is restated here rather than called, because it is private to core; core's own
    /// tests pin the other half of it. What this test owns is that **this** client never hands it a
    /// non-zero pair unless the user asked for one.
    #[test]
    fn an_opus_session_asks_for_nothing_and_a_lossless_one_asks_for_something() {
        // Core's rule, verbatim: `audio_rate_hz != 0 || audio_bits != 0`.
        let asks_for_hires = |(rate_hz, bits): (u32, u8)| rate_hz != 0 || bits != 0;

        assert!(
            !asks_for_hires(resolve_requested_audio_format(0, 0, 2)),
            "the default session must not advertise the capability"
        );
        // Surround changes nothing about it: the plane is negotiated by format, not by layout.
        assert!(!asks_for_hires(resolve_requested_audio_format(0, 0, 8)));

        // And every rung the settings screen offers does ask — otherwise the setting is inert.
        // The 48 kHz rows resolve unprobed; the rest fall to 48 kHz off-device (see above) and
        // still ask, because the fallback keeps the DEPTH and a 24-bit request is a real one.
        for rate in [44_100u32, SAMPLE_RATE_HZ, 88_200, 96_000, 176_400] {
            let wire = resolve_requested_audio_format(rate, BITS_24, 2);
            assert!(
                asks_for_hires(wire),
                "{rate} Hz / 24-bit resolved to {wire:?}, which asks for nothing"
            );
            assert_eq!(wire.1, BITS_24, "the depth must survive every fallback");
        }
    }

    /// The fallback ladder's shape, which decides what a device that refuses the user's rate is
    /// asked for next — and which is the only place this client makes a quality choice on the
    /// user's behalf, so it is worth pinning rather than reading.
    #[test]
    fn the_rate_ladder_descends_its_own_family_and_ends_at_the_48_khz_floor() {
        for rate in [44_100u32, SAMPLE_RATE_HZ, 88_200, 96_000, 176_400] {
            let ladder = rate_fallback_ladder(rate);
            assert_eq!(ladder[0], rate, "{rate} Hz must ask for itself first");
            assert_eq!(
                ladder.last().copied(),
                Some(SAMPLE_RATE_HZ),
                "{rate} Hz must end at the floor every Android output grants"
            );
            for &rung in ladder {
                assert!(
                    punktfunk_core::audio::pcm::rate_is_supported(rung),
                    "{rung} Hz is on {rate} Hz's ladder but is not a rate the plane carries"
                );
            }
            // Strictly descending, so a fallback is never an upgrade in cost — except for the
            // 48 kHz floor itself, which is above 44 100 and is the crossing the ladder makes on
            // purpose (see `rate_fallback_ladder`).
            for w in ladder.windows(2) {
                assert!(
                    w[1] < w[0] || w[1] == SAMPLE_RATE_HZ,
                    "{rate} Hz's ladder goes up at {:?}",
                    w
                );
            }
        }
        // The 44.1 family stays in the 44.1 family for as long as it can: a 176.4 kHz endpoint
        // that will not open is far likelier to be an 88.2 or 44.1 kHz one than a 96 kHz one.
        assert_eq!(
            rate_fallback_ladder(176_400),
            &[176_400, 88_200, 44_100, SAMPLE_RATE_HZ]
        );
        assert_eq!(rate_fallback_ladder(96_000), &[96_000, SAMPLE_RATE_HZ]);
        // A default session's ladder is one rung, so it opens no probe stream at all.
        assert_eq!(rate_fallback_ladder(SAMPLE_RATE_HZ), &[SAMPLE_RATE_HZ]);
    }
}
