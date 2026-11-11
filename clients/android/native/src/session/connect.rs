//! Connect lifecycle + the trust surface: identity mint, connect (TOFU / pinned), close,
//! host-fingerprint read, and the SPAKE2 PIN pairing ceremony.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JByteArray, JObject, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::EnvUnowned;
use punktfunk_core::client::{ConnectParams, NativeClient};
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use punktfunk_core::quic::v2::msg::{ProfileUnlock, Zeroizing};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{hex, jni_guard, lock_recover, parse_hex32, SessionHandle, SESSIONS};

/// Machine token of the most recent `nativeConnect`/`nativePair` failure, taken (and cleared)
/// by `nativeTakeLastError` so Kotlin can render a cause-specific message instead of the old
/// catch-all "wrong PIN, or the host isn't armed" (which blamed the PIN for dead network paths
/// — the moko0878-class support threads). The app runs one attempt at a time, so one slot
/// suffices; a stale token is harmless (it is taken immediately after the failed call).
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());
/// What the host said with that failure's refusal, beside its token; empty when it said nothing.
static LAST_SAID: Mutex<String> = Mutex::new(String::new());
/// The dial `nativeConnect` is blocked on was parked by the host for its operator.
static DIAL_PENDING: AtomicBool = AtomicBool::new(false);

/// The stable token and the host's sentence for a failed pair or connect, matched by Kotlin
/// (`ConnectErrors.kt`). A typed host rejection gives its `RejectReason::as_str()` token
/// ("not-armed", "denied", "approval-timeout", …) and what the host said; transport causes give
/// "crypto", "timeout" or "io", anything else "error", each with no sentence.
fn error_token(e: &punktfunk_core::error::PunktfunkError) -> (&str, &str) {
    use punktfunk_core::error::PunktfunkError as E;
    match e {
        E::Rejected(r, said) => (r.as_str(), said.as_str()),
        E::Crypto => ("crypto", ""),
        E::Timeout => ("timeout", ""),
        E::Io(_) => ("io", ""),
        _ => ("error", ""),
    }
}

/// Keep `e`'s token and sentence for `nativeTakeLastError` and `nativeTakeLastSaid`.
fn note_error(e: &punktfunk_core::error::PunktfunkError) {
    let (token, said) = error_token(e);
    *lock_recover(&LAST_ERROR) = token.to_string();
    *lock_recover(&LAST_SAID) = said.to_string();
}

/// `NativeBridge.nativeTakeLastSaid(): String` — the host's own sentence with the most recent
/// refusal, cleared on read (`""` when it said none). Read beside `nativeTakeLastError`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeTakeLastSaid<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> JString<'local> {
    let said = std::mem::take(&mut *lock_recover(&LAST_SAID));
    env.with_env(|env| env.new_string(said))
        .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeDialPending(): Boolean` — the dial `nativeConnect` is blocked on was
/// parked by the host for its operator's approval; it now waits for that verdict.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeDialPending(
    _env: EnvUnowned,
    _this: JObject,
) -> jboolean {
    DIAL_PENDING.load(Ordering::Relaxed)
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
fn force_parts_sysprop() -> bool {
    crate::sys::sysprop(c"debug.punktfunk.force_parts").as_deref() == Some("1")
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
            "audio: this device can't play {rate_hz} Hz without resampling, so the session asks for {granted} Hz / {bits}-bit instead — the wire is only ever offered a format this client has proved it can play"
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

/// The `Hello`'s `CLIENT_CAP_*` bits for the session-level asks.
///
/// `pad_audio_ok` is the session negotiation, separate from the per-pad arrival bits: without it
/// the host never emits the pad-audio plane. It follows the setting, so a user with pad audio
/// off does not make the host provision endpoints.
///
/// `keep_host_audio` has the host tap its default playback device rather than park the desktop
/// mix on a silent endpoint. It is a request with no host echo; an older host ignores it.
///
/// `client_cursor` is Desktop mouse mode with a mouse: Kotlin's `HostCursor` wears the host's
/// pointer and says when the host composites it instead.
fn client_caps(pad_audio_ok: bool, keep_host_audio: bool, client_cursor: bool) -> u8 {
    use punktfunk_core::quic::{
        CLIENT_CAP_CURSOR, CLIENT_CAP_KEEP_HOST_AUDIO, CLIENT_CAP_PAD_AUDIO,
    };
    let mut caps = 0;
    if pad_audio_ok {
        caps |= CLIENT_CAP_PAD_AUDIO;
    }
    if keep_host_audio {
        caps |= CLIENT_CAP_KEEP_HOST_AUDIO;
    }
    if client_cursor {
        caps |= CLIENT_CAP_CURSOR;
    }
    caps
}

/// The `CODEC_*` bits the `Hello` advertises: Kotlin's mask cut to the known codecs, and
/// H.264|HEVC when none is left, so a bogus mask cannot advertise nothing and kill the
/// handshake. PyroWave is the one bit naming no MediaCodec; it decodes as Vulkan compute.
fn advertised_codecs(bits: u8) -> u8 {
    use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE};
    let known = bits & (CODEC_H264 | CODEC_HEVC | CODEC_AV1 | CODEC_PYROWAVE);
    if known == 0 {
        CODEC_H264 | CODEC_HEVC
    } else {
        known
    }
}

/// `s` unless it is empty: an empty launch id or profile asks for nothing, as `null` does.
fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty())
}

/// The label the host files this device under: trimmed, and none when blank.
fn device_label(name: Option<String>) -> Option<String> {
    non_empty(name.map(|s| s.trim().to_string()))
}

/// The persistent identity as `(cert, key)` PEMs; anonymous unless both are present.
fn identity_of(cert: String, key: String) -> Option<(String, String)> {
    if cert.is_empty() || key.is_empty() {
        None
    } else {
        Some((cert, key))
    }
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
    /// PyroWave quality in hundredths of a bit per pixel; 0 = the host's own.
    #[serde(default)]
    pyrowave_bpp_x100: u16,
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
    /// `2` dials a network check's probes-only session (`EXT_DELIVERY_PROBE_ONLY`).
    #[serde(default)]
    delivery_flags: u8,
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
    /// Ask for the host's cursor out of band (`CLIENT_CAP_CURSOR`): Desktop mouse mode.
    #[serde(default)]
    client_cursor: bool,
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
    /// The host profile to play as; rides the Hello as `profile`.
    #[serde(default)]
    profile: Option<String>,
    /// This display shows full-range video as studio (Kotlin's Qualcomm decoder probe): the
    /// `FEATURE_STUDIO_RANGE` ask.
    #[serde(default)]
    studio_range: bool,
    /// The size the picture is shown at when the mode is larger ([`ConnectParams::view`]);
    /// `0`×`0` names none.
    #[serde(default)]
    view_width: u32,
    #[serde(default)]
    view_height: u32,
}

/// A `w`×`h` view, or `None` when either side is not positive.
fn view_of(w: i64, h: i64) -> Option<(u32, u32)> {
    Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?)).filter(|&(w, h)| w > 0 && h > 0)
}

/// The request Kotlin's JSON names; `None` (logged) when it doesn't parse.
fn request_of(env: &jni::Env, request: &JString) -> jni::errors::Result<Option<ConnectRequest>> {
    let text = request.try_to_string(env)?;
    Ok(match serde_json::from_str::<ConnectRequest>(&text) {
        Ok(r) => Some(r),
        Err(e) => {
            log::error!("nativeConnect: bad request JSON from Kotlin: {e}");
            None
        }
    })
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
        .with_env(|env| request_of(env, &request))
        .resolve::<LogErrorAndDefault>();
    let Some(req) = req else {
        return 0;
    };
    jni_guard(0, || connect(req, false, None))
}

/// `NativeBridge.nativeConnectWithUnlock(requestJson, promptsUnlock, kind, secret, staySignedIn,
/// enrollAlg, enrollKey): Long` — `nativeConnect` for a profile on a person's own account.
/// `promptsUnlock`: this app asks for the profile's password and dials again with it after the
/// `profile-locked` refusal. `secret` null sends no unlock; else `kind` 1 is the password as
/// typed (UTF-8) and 2 a key proof, and `enrollKey` is null or the key to enrol. The secret is
/// copied into a buffer that wipes; Kotlin wipes its own array once this returns. An unlock
/// outside the wire's bounds, or one whose request has no `pin_hex`, fails with the token
/// "error" before anything is dialed.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConnectWithUnlock<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    request: JString<'local>,
    prompts_unlock: jboolean,
    kind: jint,
    secret: JByteArray<'local>,
    stay_signed_in: jboolean,
    enroll_alg: jint,
    enroll_key: JByteArray<'local>,
) -> jlong {
    type Read = Option<(
        ConnectRequest,
        Option<punktfunk_core::error::Result<ProfileUnlock>>,
    )>;
    let read: Read = env
        .with_env(|env| -> jni::errors::Result<Read> {
            let Some(req) = request_of(env, &request)? else {
                return Ok(None);
            };
            if secret.is_null() {
                return Ok(Some((req, None)));
            }
            // An unlock goes only to a pinned host; refused before its secret is read.
            if parse_hex32(&req.pin_hex).is_none() {
                let unpinned = punktfunk_core::error::PunktfunkError::InvalidArg(
                    "a profile unlock needs a pinned host",
                );
                return Ok(Some((req, Some(Err(unpinned)))));
            }
            let secret = Zeroizing::new(env.convert_byte_array(&secret)?);
            let key = match enroll_key.is_null() {
                true => Vec::new(),
                false => env.convert_byte_array(&enroll_key)?,
            };
            let byte = |v: jint| u8::try_from(v).unwrap_or(u8::MAX);
            let unlock =
                ProfileUnlock::new(byte(kind), secret, stay_signed_in, byte(enroll_alg), key);
            Ok(Some((req, Some(unlock))))
        })
        .resolve::<LogErrorAndDefault>();
    let Some((req, unlock)) = read else {
        return 0;
    };
    let unlock = match unlock.transpose() {
        Ok(u) => u,
        Err(e) => {
            log::error!("nativeConnectWithUnlock: {e}");
            note_error(&e);
            return 0;
        }
    };
    jni_guard(0, || connect(req, prompts_unlock, unlock))
}

/// `NativeBridge.nativeProfileUnlockMessage(challenge, seatId, deviceFp, boxFp): ByteArray?` —
/// the 152 bytes a profile's unlock key signs, with SHA-256 as the digest
/// ([`punktfunk_core::quic::v2::msg::unlock_signed_message`]). `seatId` is the seat's 32
/// lowercase hex characters; the three arrays are 32 bytes each. Null for anything else.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeProfileUnlockMessage<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    challenge: JByteArray<'local>,
    seat_id: JString<'local>,
    device_fp: JByteArray<'local>,
    box_fp: JByteArray<'local>,
) -> JByteArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JByteArray<'local>> {
        let seat = seat_id.try_to_string(env)?;
        let fp = |a: &JByteArray| -> jni::errors::Result<Option<[u8; 32]>> {
            Ok(env.convert_byte_array(a)?.try_into().ok())
        };
        let (c, d, b) = (fp(&challenge)?, fp(&device_fp)?, fp(&box_fp)?);
        let (Some(c), Some(d), Some(b)) = (c, d, b) else {
            return Ok(JByteArray::default());
        };
        match punktfunk_core::quic::v2::msg::unlock_signed_message(&c, &seat, &d, &b) {
            Ok(m) => env.byte_array_from_slice(&m),
            Err(_) => Ok(JByteArray::default()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// The connect proper, off the JNI seam. `prompts_unlock` and `unlock` are
/// [`ConnectParams`]'s.
fn connect(req: ConnectRequest, prompts_unlock: bool, unlock: Option<ProfileUnlock>) -> jlong {
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
        pyrowave_bpp_x100,
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
        delivery_flags,
        timeout_ms,
        launch,
        device_name,
        pad_audio_ok,
        keep_host_audio,
        client_cursor,
        video_fit,
        dialer,
        preset_id,
        preset_name,
        profile,
        studio_range,
        view_width,
        view_height,
    } = req;
    // Which shell asked, for the host's `handshake complete` line. Set before the dial: core reads
    // it once the host says it parses the block.
    punktfunk_core::client::set_client_label(&dialer);
    let preset = preset_id.as_deref().and_then(|id| {
        punktfunk_core::quic::SessionPreset::new(id, preset_name.as_deref().unwrap_or(""))
    });
    let launch = non_empty(launch);
    let profile = non_empty(profile);
    let device_name = device_label(device_name);
    let identity = identity_of(cert, key);
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
        "decoder caps: multi_slice={} partial_frame={}{} hdr={} ten_bit_sdr={} studio_range={} \
         codec_bits={:#x}",
        multi_slice_ok,
        frame_parts_ok,
        if force_parts { " (FORCED by sysprop)" } else { "" },
        hdr_enabled,
        ten_bit_sdr,
        studio_range,
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
    let params = ConnectParams {
        probe_only: delivery_flags & punktfunk_core::quic::EXT_DELIVERY_PROBE_ONLY != 0,
        compositor: CompositorPref::from_u8(compositor_pref),
        gamepad: GamepadPref::from_u8(gamepad_pref),
        bitrate_kbps, // 0 = host default
        pyrowave_bpp_x100,
        // The audio supervisor reopens its device on a packet in a new format.
        reopens_audio: true,
        video_caps: video_caps(hdr_enabled, ten_bit_sdr, multi_slice_ok),
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
        audio_layout: punktfunk_core::audio::AudioLayout::Legacy,
        video_fit: punktfunk_core::video_fit::VideoFit::from_name(&video_fit),
        // Codecs this device decodes (`VideoDecoders.decodableCodecBits`). The host echoes its
        // pick in `connector.codec`.
        video_codecs: advertised_codecs(video_codecs),
        preferred_codec,
        // No display-volume forwarding from Android yet (the panel tone-maps PQ itself via the
        // Surface dataspace + static metadata) — the host keeps its virtual-display EDID defaults.
        display_hdr: None,
        client_caps: client_caps(pad_audio_ok, keep_host_audio, client_cursor),
        // Slice-progressive delivery, by decoder truth (Kotlin probes FEATURE_PartialFrame on
        // every decoder this device would use; `debug.punktfunk.force_parts` overrides for the
        // on-glass experiment): AU prefixes then arrive as `Frame::part` pieces and the decode
        // loop feeds them with BUFFER_FLAG_PARTIAL_FRAME.
        frame_parts,
        // The decode loop rebuilds its codec on a frame in a new format (`RunEnd::Reformat`).
        rebuilds_decoder: true,
        launch, // a store-qualified library id to boot into a game, or None for the desktop
        name: device_name, // Kotlin's Build.MODEL — the host's approval-list / trust-store label
        pin,    // Some → Crypto on host-fp mismatch
        identity, // owned (cert, key) PEM, or None (anonymous)
        preset,
        profile,
        // An older Qualcomm display shows full-range BT.709 as studio, so ask for studio SDR. The
        // decode path takes the range from the Welcome either way.
        studio_range,
        view: view_of(view_width.into(), view_height.into()),
        // Handshake budget from Kotlin: ~10 s for a normal connect, ~185 s for "request access"
        // (the host parks the connection until the operator approves the device — see ConnectScreen).
        // No `cancel`: Kotlin drops the result (`Dial.cancelled`) rather than abort the dial — its
        // connect runs on a pool thread, so a parked one costs a thread, not a stuck UI.
        on_pending: Some(Arc::new(|| DIAL_PENDING.store(true, Ordering::Relaxed))),
        prompts_unlock,
        unlock,
        ..ConnectParams::new(&host, port, mode, Duration::from_millis(timeout_ms))
    };
    DIAL_PENDING.store(false, Ordering::Relaxed);
    let dialed = NativeClient::connect(params);
    DIAL_PENDING.store(false, Ordering::Relaxed);
    match dialed {
        Ok(client) => {
            let client = Arc::new(client);
            // A forwarded cursor starts out of the picture: the host composites it until the
            // mouse takes it (Kotlin's HostCursor), so no session starts without a pointer.
            if client.host_caps() & punktfunk_core::quic::HOST_CAP_CURSOR != 0 {
                let _ = client.set_cursor_render(false);
            }
            let handle = SessionHandle {
                stats: Arc::new(crate::stats::VideoStats::new(client.hud_shared())),
                client,
                video: Default::default(),
                video_started: std::sync::atomic::AtomicBool::new(false),
                drain: Default::default(),
                #[cfg(target_os = "android")]
                audio: Default::default(),
                #[cfg(target_os = "android")]
                mic: Default::default(),
                #[cfg(target_os = "android")]
                pad_audio: Default::default(),
                // A fresh session is never muted (mute is per-session UI state, not a setting).
                mic_muted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                access_seq: std::sync::atomic::AtomicU32::new(0),
                // Reported by Kotlin at `surfaceCreated` and on every resize after it.
                surface_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                // The full frame until Kotlin places the picture.
                src_crop: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                decoded_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                layers: Arc::new(super::PictureLayers::new()),
            };
            SESSIONS.insert(handle)
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
/// Pad audio is joined here, since it borrows a USB fd Kotlin may close once this returns. Other
/// JNI calls keep their `Arc` until they return; the final drop joins the remaining workers and
/// closes the connector. Zero, stale, duplicate, and concurrent closes are no-ops.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClose(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        // Joined while the key still resolves: a `nativeStopPadAudio` racing this close then
        // waits on the same join instead of finding no session and returning at once.
        #[cfg(target_os = "android")]
        if let Some(session) = SESSIONS.get(handle) {
            session.stop_pad_audio();
        }
        drop(SESSIONS.remove(handle));
    })
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
        if let Some(session) = SESSIONS.get(handle) {
            session.client.disconnect_quit();
        }
    })
}

/// Request an in-stream resolution switch without reconnecting, shown at the `view_width` ×
/// `view_height` view (`request_mode_view`). A side of `0` names none, which clears a view the
/// host keeps; a negative side keeps it (`request_mode`), for a rate pick that does not know it.
///
/// Returns `false` for a missing session, nonpositive geometry, or a closed request channel.
/// A concurrent close cannot invalidate the table-retained session during this enqueue.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeRequestMode(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    width: jint,
    height: jint,
    refresh_hz: jint,
    view_width: jint,
    view_height: jint,
) -> jboolean {
    jni_guard(false, || {
        if width <= 0 || height <= 0 || refresh_hz <= 0 {
            return false;
        }
        let Some(session) = SESSIONS.get(handle) else {
            return false;
        };
        let mode = Mode {
            width: width as u32,
            height: height as u32,
            refresh_hz: refresh_hz as u32,
        };
        if view_width < 0 || view_height < 0 {
            return session.client.request_mode(mode).is_ok();
        }
        let view = view_of(view_width.into(), view_height.into());
        session.client.request_mode_view(mode, view).is_ok()
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
    let out = SESSIONS
        .get(handle)
        .map(|session| hex(&session.client.host_fingerprint))
        .unwrap_or_default();
    env.with_env(|env| env.new_string(out))
        .resolve::<LogErrorAndDefault>()
}

/// Report whether the keyed session started on a screen another device opened and sized.
/// Missing or concurrently closed keys, and a host that does not say, return `false`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSharesScreen(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    use punktfunk_core::quic::v2::hello::ScreenEcho;
    jni_guard(false, || {
        SESSIONS
            .get(handle)
            .is_some_and(|session| session.client.screen() == Some(&ScreenEcho::Shared))
    })
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
        SESSIONS
            .get(handle)
            .is_some_and(|session| session.client.is_session_ended())
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
        SESSIONS
            .get(handle)
            .map(|session| session.client.end_reason() as jint)
            .unwrap_or(0)
    })
}

/// `NativeBridge.nativeRedialDelayMs(endReason, attempt)` — milliseconds before redial `attempt`
/// (0-based) of a session that ended with `endReason`, on the schedule every client shares; `-1`
/// means stay ended. Only a lost connection redials.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeRedialDelayMs(
    _env: EnvUnowned,
    _this: JObject,
    end_reason: jint,
    attempt: jint,
) -> jlong {
    jni_guard(-1, || {
        punktfunk_core::client::PunktfunkEndReason::from_u8(end_reason as u8)
            .redial_delay(attempt.max(0) as u32)
            .map_or(-1, |d| d.as_millis() as jlong)
    })
}

/// `sinceFirstMs` as the core reads it: negative is no refusal yet.
fn since_first(since_first_ms: jlong) -> Option<Duration> {
    u64::try_from(since_first_ms)
        .ok()
        .map(Duration::from_millis)
}

/// `NativeBridge.nativeWakeRedialDelayMs(autoWake, token, sinceFirstMs)` — milliseconds before
/// dialing again a host that refused a connect with `token` (`nativeTakeLastError`),
/// `sinceFirstMs` after its first such refusal (`-1`: the first); `-1` means show the refusal.
/// The rule every client shares (`punktfunk_core::reject::wake_redial_delay`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeWakeRedialDelayMs<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    auto_wake: jboolean,
    token: JString<'local>,
    since_first_ms: jlong,
) -> jlong {
    let token = env
        .with_env(|env| token.try_to_string(env))
        .resolve::<LogErrorAndDefault>();
    jni_guard(-1, || {
        let refused = punktfunk_core::reject::RejectReason::from_token(&token);
        punktfunk_core::reject::wake_redial_delay(
            auto_wake,
            refused,
            false,
            since_first(since_first_ms),
        )
        .map_or(-1, |d| d.as_millis() as jlong)
    })
}

/// `NativeBridge.nativeSessionWakeRedialDelayMs(handle, autoWake, sinceFirstMs)` — the same for
/// a session that ended: its typed close counts only before any frame. `-1` for a missing key.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSessionWakeRedialDelayMs(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    auto_wake: jboolean,
    since_first_ms: jlong,
) -> jlong {
    jni_guard(-1, || {
        SESSIONS
            .get(handle)
            .and_then(|s| {
                s.client
                    .wake_redial_delay(auto_wake, since_first(since_first_ms))
            })
            .map_or(-1, |d| d.as_millis() as jlong)
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
                Ok(host_fp) => hex(&host_fp),
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

/// `NativeBridge.nativeRequestAccess(host, port, certPem, keyPem, pinHex, name): String` — ask
/// for access with no PIN and wait for the host's operator. `pinHex` is the advertised
/// fingerprint, `""` to trust on first use. Returns the host fingerprint (64-hex) to persist as
/// paired, or `""` with the cause in `nativeTakeLastError`. Never streams. Blocking.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeRequestAccess<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    host: JString<'local>,
    port: jint,
    cert_pem: JString<'local>,
    key_pem: JString<'local>,
    pin_hex: JString<'local>,
    name: JString<'local>,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let g = |e: &jni::Env<'local>, j: &JString<'local>| -> String {
            j.try_to_string(e).unwrap_or_default()
        };
        let host = g(env, &host);
        let cert = g(env, &cert_pem);
        let key = g(env, &key_pem);
        let pin = punktfunk_core::fp::parse_hex32(&g(env, &pin_hex));
        let name = g(env, &name);

        let out = if host.is_empty() || cert.is_empty() || key.is_empty() {
            log::error!("nativeRequestAccess: missing host/identity");
            String::new()
        } else {
            match NativeClient::request_access(
                &host,
                port as u16,
                (&cert, &key),
                pin,
                &name,
                pf_client_core::trust::REQUEST_ACCESS_TIMEOUT,
                None,
            ) {
                Ok(host_fp) => hex(&host_fp),
                Err(e) => {
                    log::error!("nativeRequestAccess to {host}:{port} failed: {e}");
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

/// `clients/shared/connect-request-vectors.json`: the JSON Kotlin writes. The struct reads every
/// key through `#[serde(default)]` and ignores unknown ones, so a key renamed on either side
/// reads as its default; these tests fail instead.
#[cfg(test)]
mod request_vectors {
    use super::ConnectRequest;
    use serde::de::{self, Deserialize, Deserializer, Visitor};
    use std::collections::BTreeSet;

    /// The key names serde derived for [`ConnectRequest`], read from the `fields` the derive
    /// hands `deserialize_struct`.
    fn field_names() -> BTreeSet<String> {
        struct Fields;
        impl<'de> Deserializer<'de> for Fields {
            type Error = de::value::Error;
            fn deserialize_struct<V: Visitor<'de>>(
                self,
                _name: &'static str,
                fields: &'static [&'static str],
                _visitor: V,
            ) -> Result<V::Value, Self::Error> {
                Err(de::Error::custom(fields.join(",")))
            }
            fn deserialize_any<V: Visitor<'de>>(
                self,
                _visitor: V,
            ) -> Result<V::Value, Self::Error> {
                Err(de::Error::custom("not a struct"))
            }
            serde::forward_to_deserialize_any! {
                bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
                byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map enum
                identifier ignored_any
            }
        }
        let err = ConnectRequest::deserialize(Fields)
            .err()
            .expect("a struct reads fields");
        err.to_string().split(',').map(str::to_owned).collect()
    }

    fn cases() -> Vec<serde_json::Value> {
        let file: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../shared/connect-request-vectors.json"
        ))
        .unwrap();
        file["cases"].as_array().expect("cases").clone()
    }

    #[test]
    fn every_vector_key_is_a_field_and_every_field_is_in_a_vector() {
        let known = field_names();
        assert!(
            known.contains("host") && known.contains("timeout_ms"),
            "{known:?}"
        );
        let mut seen = BTreeSet::new();
        for case in cases() {
            let name = &case["name"];
            for key in case["json"].as_object().expect("json").keys() {
                assert!(
                    known.contains(key),
                    "{name}: ConnectRequest has no field `{key}`, so serde drops it"
                );
                seen.insert(key.clone());
            }
        }
        let unwritten: Vec<_> = known.difference(&seen).collect();
        assert!(unwritten.is_empty(), "no vector writes {unwritten:?}");
    }

    #[test]
    fn every_vector_parses() {
        for case in cases() {
            let text = case["json"].to_string();
            if let Err(e) = serde_json::from_str::<ConnectRequest>(&text) {
                panic!("{}: {e}", case["name"]);
            }
        }
    }
}

/// The decoders `connect` and `note_error` run over Kotlin's request: what reaches the `Hello`
/// and what Kotlin reads back as the failure token.
#[cfg(test)]
mod decoder_tests {
    use super::*;
    use punktfunk_core::error::PunktfunkError;
    use punktfunk_core::quic::{
        CLIENT_CAP_CURSOR, CLIENT_CAP_KEEP_HOST_AUDIO, CLIENT_CAP_PAD_AUDIO, CODEC_AV1, CODEC_H264,
        CODEC_HEVC, CODEC_PYROWAVE,
    };
    use punktfunk_core::reject::RejectReason;

    /// Tokens Kotlin reads for a transport failure; no typed rejection may take one.
    const TRANSPORT_TOKENS: [&str; 4] = ["crypto", "timeout", "io", "error"];

    fn rejected(reason: RejectReason, said: &str) -> PunktfunkError {
        PunktfunkError::Rejected(reason, said.to_string())
    }

    #[test]
    fn a_failure_maps_to_the_token_kotlin_matches_and_the_hosts_sentence() {
        // (name, error, token, said)
        let rows: Vec<(&str, PunktfunkError, &str, &str)> = vec![
            (
                "a rejection with no sentence",
                rejected(RejectReason::PairingNotArmed, ""),
                "not-armed",
                "",
            ),
            (
                "a rejection carries what the host said",
                rejected(RejectReason::Busy, "someone is streaming right now"),
                "busy",
                "someone is streaming right now",
            ),
            (
                "a locked profile",
                rejected(RejectReason::ProfileLocked, ""),
                "profile-locked",
                "",
            ),
            (
                "a wrong PIN or a changed host",
                PunktfunkError::Crypto,
                "crypto",
                "",
            ),
            (
                "a handshake timeout",
                PunktfunkError::Timeout,
                "timeout",
                "",
            ),
            (
                "a socket failure",
                PunktfunkError::Io(std::io::Error::other("unreachable")),
                "io",
                "",
            ),
            (
                "an invalid argument",
                PunktfunkError::InvalidArg("x"),
                "error",
                "",
            ),
            ("a malformed packet", PunktfunkError::BadPacket, "error", ""),
            ("no frame", PunktfunkError::NoFrame, "error", ""),
            (
                "an unsupported request",
                PunktfunkError::Unsupported("x"),
                "error",
                "",
            ),
            ("a closed session", PunktfunkError::Closed, "error", ""),
        ];
        for (name, err, token, said) in rows {
            assert_eq!(error_token(&err), (token, said), "{name}");
        }
    }

    #[test]
    fn every_typed_rejection_keeps_its_own_token_and_never_a_transport_one() {
        for &reason in RejectReason::ALL {
            let err = rejected(reason, "said");
            let (token, said) = error_token(&err);
            assert_eq!(token, reason.as_str(), "{reason:?}");
            assert_eq!(said, "said", "{reason:?}");
            assert!(
                !TRANSPORT_TOKENS.contains(&token),
                "{reason:?} reads as a transport failure"
            );
        }
    }

    /// The one test that writes `LAST_ERROR` and `LAST_SAID`, so it cannot race another.
    #[test]
    fn a_later_failure_without_a_sentence_clears_the_earlier_sentence() {
        let kept = || {
            (
                lock_recover(&LAST_ERROR).clone(),
                lock_recover(&LAST_SAID).clone(),
            )
        };
        note_error(&rejected(RejectReason::Busy, "one session at a time"));
        assert_eq!(kept(), ("busy".into(), "one session at a time".into()));
        note_error(&PunktfunkError::Timeout);
        assert_eq!(kept(), ("timeout".into(), String::new()));
    }

    #[test]
    fn the_caps_mask_is_the_union_of_the_asks_made() {
        // (name, pad audio, keep host audio, host cursor, bits)
        const CASES: &[(&str, bool, bool, bool, u8)] = &[
            ("no asks", false, false, false, 0),
            ("pad audio", true, false, false, CLIENT_CAP_PAD_AUDIO),
            (
                "keep host audio",
                false,
                true,
                false,
                CLIENT_CAP_KEEP_HOST_AUDIO,
            ),
            ("host cursor", false, false, true, CLIENT_CAP_CURSOR),
            (
                "pad audio and keep host audio",
                true,
                true,
                false,
                CLIENT_CAP_PAD_AUDIO | CLIENT_CAP_KEEP_HOST_AUDIO,
            ),
            (
                "pad audio and host cursor",
                true,
                false,
                true,
                CLIENT_CAP_PAD_AUDIO | CLIENT_CAP_CURSOR,
            ),
            (
                "keep host audio and host cursor",
                false,
                true,
                true,
                CLIENT_CAP_KEEP_HOST_AUDIO | CLIENT_CAP_CURSOR,
            ),
            (
                "all three",
                true,
                true,
                true,
                CLIENT_CAP_PAD_AUDIO | CLIENT_CAP_KEEP_HOST_AUDIO | CLIENT_CAP_CURSOR,
            ),
        ];
        for &(name, pad, keep, cursor, want) in CASES {
            assert_eq!(client_caps(pad, keep, cursor), want, "{name}");
        }
        let bits = [
            CLIENT_CAP_PAD_AUDIO,
            CLIENT_CAP_KEEP_HOST_AUDIO,
            CLIENT_CAP_CURSOR,
        ];
        assert_eq!(
            bits.iter().fold(0u8, |all, &b| all | b).count_ones(),
            3,
            "each ask has a bit of its own"
        );
    }

    #[test]
    fn a_codec_mask_is_cut_to_the_known_codecs_and_falls_back_to_h264_and_hevc() {
        const FALLBACK: u8 = CODEC_H264 | CODEC_HEVC;
        // (name, Kotlin's mask, advertised)
        const CASES: &[(&str, u8, u8)] = &[
            ("an empty mask", 0, FALLBACK),
            ("unknown bits only", 0xF0, FALLBACK),
            ("the top bit only", 0x80, FALLBACK),
            ("H.264 alone is not widened", CODEC_H264, CODEC_H264),
            ("HEVC alone is not widened", CODEC_HEVC, CODEC_HEVC),
            ("AV1 alone", CODEC_AV1, CODEC_AV1),
            ("PyroWave alone", CODEC_PYROWAVE, CODEC_PYROWAVE),
            (
                "every known codec",
                CODEC_H264 | CODEC_HEVC | CODEC_AV1 | CODEC_PYROWAVE,
                CODEC_H264 | CODEC_HEVC | CODEC_AV1 | CODEC_PYROWAVE,
            ),
            (
                "every bit set",
                0xFF,
                CODEC_H264 | CODEC_HEVC | CODEC_AV1 | CODEC_PYROWAVE,
            ),
            (
                "unknown bits beside known ones",
                0xF0 | CODEC_AV1 | CODEC_PYROWAVE,
                CODEC_AV1 | CODEC_PYROWAVE,
            ),
            ("an unknown bit beside H.264", 0x10 | CODEC_H264, CODEC_H264),
        ];
        for &(name, bits, want) in CASES {
            assert_eq!(advertised_codecs(bits), want, "{name}");
        }
    }

    #[test]
    fn an_empty_launch_or_profile_is_none_and_is_not_trimmed() {
        let some = |s: &str| Some(s.to_string());
        let rows = [
            ("absent", None, None),
            ("empty", some(""), None),
            (
                "a store-qualified id",
                some("steam:1245620"),
                some("steam:1245620"),
            ),
            ("blank is kept", some(" "), some(" ")),
            ("padding is kept", some(" steam:1 "), some(" steam:1 ")),
        ];
        for (name, input, want) in rows {
            assert_eq!(non_empty(input), want, "{name}");
        }
    }

    #[test]
    fn a_device_name_is_trimmed_and_blank_is_none() {
        let some = |s: &str| Some(s.to_string());
        let rows = [
            ("absent", None, None),
            ("empty", some(""), None),
            ("spaces only", some("   "), None),
            ("tab and newline only", some("\t\n"), None),
            ("a no-break space only", some("\u{a0}"), None),
            ("a model name", some("Pixel 9"), some("Pixel 9")),
            ("padding is trimmed", some("  Pixel 9\n"), some("Pixel 9")),
            ("inner spaces are kept", some("Pixel  9"), some("Pixel  9")),
        ];
        for (name, input, want) in rows {
            assert_eq!(device_label(input), want, "{name}");
        }
    }

    #[test]
    fn an_identity_needs_both_halves() {
        let pair = |c: &str, k: &str| identity_of(c.to_string(), k.to_string());
        assert_eq!(pair("", ""), None, "both empty");
        assert_eq!(pair("cert", ""), None, "no key");
        assert_eq!(pair("", "key"), None, "no cert");
        assert_eq!(
            pair("cert", "key"),
            Some(("cert".to_string(), "key".to_string())),
            "both present"
        );
        assert!(
            pair(" ", " ").is_some(),
            "blank is not empty, and is not trimmed"
        );
    }

    #[test]
    fn a_negative_since_first_is_no_refusal_yet() {
        const CASES: &[(&str, jlong, Option<Duration>)] = &[
            ("i64::MIN", jlong::MIN, None),
            ("the first refusal", -1, None),
            ("no time since", 0, Some(Duration::ZERO)),
            (
                "a second and a half",
                1_500,
                Some(Duration::from_millis(1_500)),
            ),
            (
                "i64::MAX",
                jlong::MAX,
                Some(Duration::from_millis(jlong::MAX as u64)),
            ),
        ];
        for &(name, ms, want) in CASES {
            assert_eq!(since_first(ms), want, "{name}");
        }
    }
}
