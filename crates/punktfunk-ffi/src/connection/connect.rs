//! Dialing a host: every `punktfunk_connect*` entry point and their options, plus
//! pairing, identity generation and the pre-connect probe.

#[cfg(feature = "quic")]
use super::planes::AudioPcmState;
#[cfg(feature = "quic")]
use crate::*;

/// Trust: `pin_sha256` (NULL or 32 bytes) is the expected SHA-256 of the host
/// certificate — a mismatch is rejected. NULL = trust on first use; persist
/// `observed_sha256_out` (NULL or 32 bytes, filled on success) and pass it as
/// the pin on every later connect.
///
/// Identity: `client_cert_pem`/`client_key_pem` (both NULL, or both NUL-terminated
/// PEM — [`punktfunk_generate_identity`]) are TLS client auth so a host can
/// recognize this client once paired ([`punktfunk_pair`]). NULL = anonymous;
/// `--require-pairing` hosts reject anonymous sessions.
///
/// # Safety
/// `host` is a NUL-terminated UTF-8 string (IP or resolvable hostname);
/// `pin_sha256`/`observed_sha256_out` are each NULL or valid for 32 bytes;
/// `client_cert_pem`/`client_key_pem` are each NULL or NUL-terminated UTF-8.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connect(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        timeout_ms,
        ..legacy_opts()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, std::ptr::null_mut()) }
}

/// [`punktfunk_connect`] plus a `compositor` (`PUNKTFUNK_COMPOSITOR_*`). `AUTO`
/// (or unrecognized) lets the host decide; a concrete value is honored only if
/// available. Same as [`punktfunk_connect_ex2`] with `gamepad = PUNKTFUNK_GAMEPAD_AUTO`.
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connect_ex(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        compositor,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        timeout_ms,
        ..legacy_opts()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, std::ptr::null_mut()) }
}

/// [`punktfunk_connect_ex`] plus a virtual `gamepad` (`PUNKTFUNK_GAMEPAD_*`).
/// `AUTO` (or unrecognized) lets the host decide (`PUNKTFUNK_GAMEPAD` env, else
/// X-Box 360). Resolved via [`punktfunk_connection_gamepad`]. Only DualSense
/// emits HID-output feedback.
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connect_ex2(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        compositor,
        gamepad,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        timeout_ms,
        ..legacy_opts()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, std::ptr::null_mut()) }
}

/// [`punktfunk_connect_ex2`] plus encoder `bitrate_kbps`. `0` = host default;
/// other values clamp to the host range. Read [`punktfunk_connection_bitrate`].
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connect_ex3(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        compositor,
        gamepad,
        bitrate_kbps,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        timeout_ms,
        ..legacy_opts()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, std::ptr::null_mut()) }
}

/// [`punktfunk_connect_ex3`] plus a library title. `launch_id` is a store-qualified
/// id (`steam:<appid>` / `custom:<id>`); the host resolves it against its own
/// library. `NULL` / empty / unknown ⇒ default session, no game.
///
/// # Safety
/// Same as [`punktfunk_connect`]; non-NULL `launch_id` is a NUL-terminated C string.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connect_ex4(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        compositor,
        gamepad,
        bitrate_kbps,
        launch_id,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        timeout_ms,
        ..legacy_opts()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, std::ptr::null_mut()) }
}

/// [`punktfunk_connect_ex4`] plus `video_caps` (`PUNKTFUNK_VIDEO_CAP_*`).
/// Host upgrades only when the bit is set. Read colour via
/// [`punktfunk_connection_color_info`] / [`punktfunk_connection_next_hdr_meta`].
///
/// # Safety
/// Same as [`punktfunk_connect`]; non-NULL `launch_id` is a NUL-terminated C string.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex5(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        compositor,
        gamepad,
        bitrate_kbps,
        video_caps,
        launch_id,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        timeout_ms,
        ..legacy_opts()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, std::ptr::null_mut()) }
}

/// [`punktfunk_connect_ex5`] plus audio channel count: `2` (stereo), `6` (5.1) or
/// `8` (7.1). Host clamps to what it can capture; read
/// [`punktfunk_connection_audio_channels`]. Advertises HEVC-only, no codec
/// preference ([`punktfunk_connect_ex7`] negotiates).
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex6(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        compositor,
        gamepad,
        bitrate_kbps,
        video_caps,
        audio_channels,
        launch_id,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        timeout_ms,
        ..legacy_opts()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, std::ptr::null_mut()) }
}

/// [`punktfunk_connect_ex6`] plus `video_codecs` (`PUNKTFUNK_CODEC_*` bits) and a
/// soft `preferred_codec` (one codec bit, `0` = none). Host honors preference when
/// it can produce it, else best shared codec. Read [`punktfunk_connection_codec`].
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex7(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    video_codecs: u8,
    preferred_codec: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        compositor,
        gamepad,
        bitrate_kbps,
        video_caps,
        audio_channels,
        video_codecs,
        preferred_codec,
        launch_id,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        timeout_ms,
        ..Default::default()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, std::ptr::null_mut()) }
}

/// [`punktfunk_connect_ex7`] plus `status_out` (nullable): the mapped
/// [`PunktfunkStatus`], including typed host rejections. NULL alone cannot say why.
///
/// # Safety
/// Same as [`punktfunk_connect`]; non-null `status_out` points to a writable `i32`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex8(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    video_codecs: u8,
    preferred_codec: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        compositor,
        gamepad,
        bitrate_kbps,
        video_caps,
        audio_channels,
        video_codecs,
        preferred_codec,
        launch_id,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        timeout_ms,
        ..Default::default()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, status_out) }
}

/// [`punktfunk_connect_ex8`] plus `client_caps`. Cursor bit: host stops compositing;
/// the embedder must drain shape/state or there is no pointer. Pass 0 for composited.
///
/// # Safety
/// Same as [`punktfunk_connect_ex8`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex9(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    video_codecs: u8,
    preferred_codec: u8,
    client_caps: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        compositor,
        gamepad,
        bitrate_kbps,
        video_caps,
        audio_channels,
        video_codecs,
        preferred_codec,
        client_caps,
        launch_id,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        timeout_ms,
        ..Default::default()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, status_out) }
}

/// [`punktfunk_connect_ex9`] plus `device_name` — the label this device knocks
/// with. NULL/empty = [`punktfunk_core::client::device_name`]. Longer than
/// [`HELLO_NAME_MAX`] is truncated on a character boundary, not rejected.
///
/// # Safety
/// Same as [`punktfunk_connect_ex9`]; non-null `device_name` is a NUL-terminated C string.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex10(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    video_codecs: u8,
    preferred_codec: u8,
    client_caps: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    device_name: *const std::os::raw::c_char,
    timeout_ms: u32,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        compositor,
        gamepad,
        bitrate_kbps,
        video_caps,
        audio_channels,
        video_codecs,
        preferred_codec,
        client_caps,
        launch_id,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        device_name,
        timeout_ms,
        ..Default::default()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, status_out) }
}

/// [`punktfunk_connect_ex10`] plus an audio-format ask (`audio_rate_hz` /
/// `audio_bits`). Any non-zero pair — including `48000`/`16` — sets
/// `CLIENT_CAP_AUDIO_HIRES` and asks for lossless `0xD3`. `0`/`0` is unspecified
/// (Opus). Do not pass 48 kHz/16 as a stand-in for default.
///
/// The host may still resolve Opus (`design/hi-res-audio.md`); open the device
/// from [`punktfunk_connection_audio_sample_rate`] / `_bits`, not from the ask.
/// At 44.1 kHz, [`punktfunk_connection_audio_frame_us`] is a nominal length —
/// advance clocks from samples / rate, not from that figure.
///
/// # Safety
/// Same as [`punktfunk_connect_ex10`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex11(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    audio_rate_hz: u32,
    audio_bits: u8,
    video_codecs: u8,
    preferred_codec: u8,
    client_caps: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    device_name: *const std::os::raw::c_char,
    timeout_ms: u32,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        compositor,
        gamepad,
        bitrate_kbps,
        video_caps,
        audio_channels,
        audio_rate_hz,
        audio_bits,
        video_codecs,
        preferred_codec,
        client_caps,
        launch_id,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        device_name,
        timeout_ms,
        ..Default::default()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, status_out) }
}

/// [`punktfunk_connect_ex11`] plus `video_fit`: how this client fills its view when the frame's
/// shape differs (`PUNKTFUNK_VIDEO_FIT_*`; unknown = fit). A host that frames the picture for
/// another device reframes to it. Every other argument is [`punktfunk_connect_ex11`]'s.
///
/// Frozen, like the rest of the `connect_ex*` family: new options land only in
/// [`PunktfunkConnectOpts`]. Prefer [`punktfunk_connect_opts`].
///
/// # Safety
/// Same as [`punktfunk_connect_ex10`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex12(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    audio_rate_hz: u32,
    audio_bits: u8,
    video_codecs: u8,
    preferred_codec: u8,
    client_caps: u8,
    video_fit: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    device_name: *const std::os::raw::c_char,
    timeout_ms: u32,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    let o = PunktfunkConnectOpts {
        host,
        port,
        width,
        height,
        refresh_hz,
        compositor,
        gamepad,
        bitrate_kbps,
        video_caps,
        audio_channels,
        audio_rate_hz,
        audio_bits,
        video_codecs,
        preferred_codec,
        client_caps,
        video_fit,
        launch_id,
        pin_sha256,
        client_cert_pem,
        client_key_pem,
        device_name,
        timeout_ms,
        ..Default::default()
    };
    // SAFETY: the caller's pointers, moved unchanged into `o`; this shim dereferences nothing.
    unsafe { connect_ex_impl(&o, observed_sha256_out, status_out) }
}

/// [`PunktfunkConnectOpts::video_fit`]: whole picture, bars.
pub const PUNKTFUNK_VIDEO_FIT_FIT: u8 = 0;
/// [`PunktfunkConnectOpts::video_fit`]: fill the view, cut the overflow.
pub const PUNKTFUNK_VIDEO_FIT_CROP: u8 = 1;
/// [`PunktfunkConnectOpts::video_fit`]: fill the view, scale each axis alone.
pub const PUNKTFUNK_VIDEO_FIT_STRETCH: u8 = 2;

/// [`punktfunk_connect_ex9`] `client_caps` bit: render the host cursor locally
/// (`design/remote-desktop-sweep.md`).
pub const PUNKTFUNK_CLIENT_CAP_CURSOR: u8 = 0x01;

/// [`punktfunk_connect_ex9`] `client_caps` bit: pad-audio plane (0xD1 — DualSense
/// voice-coil + speaker). Drain [`punktfunk_connection_next_pad_audio`] and declare
/// pads via [`punktfunk_connection_set_pad_audio_caps`]. Host emits only with
/// [`PUNKTFUNK_HOST_CAP_PAD_AUDIO`].
pub const PUNKTFUNK_CLIENT_CAP_PAD_AUDIO: u8 = 0x08;

/// Ask for lossless `0xD3`. Usually derived from a non-zero `audio_rate_hz`/
/// `audio_bits` on [`punktfunk_connect_ex11`]. Set by hand only for 48 kHz/16-bit
/// lossless (indistinguishable from an unspecified ask).
pub const PUNKTFUNK_CLIENT_CAP_AUDIO_HIRES: u8 = 0x10;

/// Keep host speakers live this session (do not park the mix). Request-only;
/// hosts that do not know the bit ignore it.
pub const PUNKTFUNK_CLIENT_CAP_KEEP_HOST_AUDIO: u8 = 0x20;

/// Cut a device name to [`HELLO_NAME_MAX`] bytes on a character boundary.
/// Truncate, don't reject: a too-long label must not fail connect.
#[cfg(feature = "quic")]
fn clamp_device_name(s: &str) -> String {
    let end = s
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&i| i <= punktfunk_core::quic::HELLO_NAME_MAX)
        .last()
        .unwrap_or(0);
    s[..end].to_string()
}

/// Growable connect options for [`punktfunk_connect_opts`]. Zero-init, set
/// `struct_size = sizeof(PunktfunkConnectOpts)`, then the fields you mean.
/// Zero = auto/unspecified (`audio_rate_hz = 0` is Opus; a non-zero pair is
/// lossless). Append only; no tail padding (sizes asserted in `punktfunk-ffi`); bump ABI.
#[cfg(feature = "quic")]
#[repr(C)]
pub struct PunktfunkConnectOpts {
    /// `sizeof(PunktfunkConnectOpts)` as this caller was compiled. Smaller than
    /// the frozen minimum is rejected; a shorter prefix defaults the tail.
    pub struct_size: u32,
    /// Required: NUL-terminated UTF-8 IP or hostname (the one non-nullable pointer here).
    pub host: *const std::os::raw::c_char,
    /// Library id to auto-launch, or null ([`punktfunk_connect_ex4`]).
    pub launch_id: *const std::os::raw::c_char,
    /// Null (trust on first use) or the host certificate's expected 32-byte SHA-256
    /// ([`punktfunk_connect`]'s trust contract).
    pub pin_sha256: *const u8,
    /// TLS client identity: both null (anonymous) or both NUL-terminated PEM
    /// ([`punktfunk_generate_identity`]).
    pub client_cert_pem: *const std::os::raw::c_char,
    /// See `client_cert_pem`.
    pub client_key_pem: *const std::os::raw::c_char,
    /// Label this device knocks with, or null for the OS default
    /// ([`punktfunk_connect_ex10`]).
    pub device_name: *const std::os::raw::c_char,
    /// Requested mode ([`punktfunk_connect`]).
    pub width: u32,
    /// See `width`.
    pub height: u32,
    /// See `width`.
    pub refresh_hz: u32,
    /// `PUNKTFUNK_COMPOSITOR_*`; `0`/unrecognized = auto ([`punktfunk_connect_ex`]).
    pub compositor: u32,
    /// `PUNKTFUNK_GAMEPAD_*`; `0`/unrecognized = auto ([`punktfunk_connect_ex2`]).
    pub gamepad: u32,
    /// Session wire budget in kbps; `0` = host default ([`punktfunk_connect_ex3`]).
    pub bitrate_kbps: u32,
    /// Audio format ask; `0`/`0` = unspecified (Opus). An explicit pair — including
    /// 48000/16 — is lossless and derives `PUNKTFUNK_CLIENT_CAP_AUDIO_HIRES`.
    pub audio_rate_hz: u32,
    /// Connect timeout in milliseconds.
    pub timeout_ms: u32,
    /// Required: the host's UDP port.
    pub port: u16,
    /// `PUNKTFUNK_VIDEO_CAP_*` bits ([`punktfunk_connect_ex5`]).
    pub video_caps: u8,
    /// Channel ask: 2 / 6 / 8; `0` = stereo ([`punktfunk_connect_ex6`]).
    pub audio_channels: u8,
    /// See `audio_rate_hz`.
    pub audio_bits: u8,
    /// `PUNKTFUNK_CODEC_*` bits the client can decode ([`punktfunk_connect_ex7`]).
    pub video_codecs: u8,
    /// The one `PUNKTFUNK_CODEC_*` bit to prefer; `0` = host's choice
    /// ([`punktfunk_connect_ex7`]).
    pub preferred_codec: u8,
    /// `PUNKTFUNK_CLIENT_CAP_*` bits ([`punktfunk_connect_ex9`]).
    pub client_caps: u8,
    /// Always `0`, ignored. Held so the struct keeps its v35 size.
    pub reserved1: u32,
    /// `PUNKTFUNK_VIDEO_FIT_*`: how this client fills its view when the frame's shape
    /// differs; unknown = fit. A host that frames the picture for another device reframes
    /// to it. v35–v40 callers zeroed this byte as `reserved0`, so they ask for fit.
    pub video_fit: u8,
    /// Always `0`. Fills what would otherwise be padding: C leaves padding unspecified
    /// even under `= {0}`, so a later field there would read a caller's garbage.
    pub reserved0: [u8; 3],
    /// The settings preset this dial names: its stable id, or null. The host shows it and
    /// hands it to hooks; the stream is unchanged. Null falls back to
    /// [`punktfunk_set_session_preset`].
    pub preset_id: *const std::os::raw::c_char,
    /// The preset's display name, or null. Read only beside a non-null `preset_id`.
    pub preset_name: *const std::os::raw::c_char,
    /// The delivery ask (`EXT_TAG_DELIVERY`): the profile on the host's record (`1` capped,
    /// `2` smooth) and the flags a network check sets (`1` facts, `2` probes only). Both `0`
    /// asks nothing, which is what a shorter prefix defaults to.
    pub delivery_profile: u8,
    /// See `delivery_profile`.
    pub delivery_flags: u8,
    /// Always `0`. Fills what would otherwise be padding, as `reserved0` does.
    pub reserved2: [u8; 6],
    /// The profile to play as: a host profile id (at most 64 bytes), or null to let the host
    /// choose. [`punktfunk_connection_profile`] reads which it resolved.
    pub profile_id: *const std::os::raw::c_char,
}

// No tail padding (append contract). On grow: freeze `CONNECT_OPTS_MIN_SIZE`, update these sizes.
// `video_fit` sits in the byte v35–v40 callers zeroed as `reserved0`.
#[cfg(feature = "quic")]
const _: () = {
    use core::mem::{offset_of, size_of};
    #[cfg(target_pointer_width = "64")]
    assert!(
        size_of::<PunktfunkConnectOpts>() == 136
            && offset_of!(PunktfunkConnectOpts, video_fit) == 100
            && offset_of!(PunktfunkConnectOpts, delivery_profile) == 120
            && offset_of!(PunktfunkConnectOpts, profile_id) == 128
    );
    #[cfg(target_pointer_width = "32")]
    assert!(
        size_of::<PunktfunkConnectOpts>() == 96
            && offset_of!(PunktfunkConnectOpts, video_fit) == 72
            && offset_of!(PunktfunkConnectOpts, delivery_profile) == 84
            && offset_of!(PunktfunkConnectOpts, profile_id) == 92
    );
};

/// All zero, as C's `= {0}` leaves it, with this build's `struct_size`.
#[cfg(feature = "quic")]
impl Default for PunktfunkConnectOpts {
    fn default() -> Self {
        PunktfunkConnectOpts {
            struct_size: std::mem::size_of::<Self>() as u32,
            host: ptr::null(),
            launch_id: ptr::null(),
            pin_sha256: ptr::null(),
            client_cert_pem: ptr::null(),
            client_key_pem: ptr::null(),
            device_name: ptr::null(),
            width: 0,
            height: 0,
            refresh_hz: 0,
            compositor: 0,
            gamepad: 0,
            bitrate_kbps: 0,
            audio_rate_hz: 0,
            timeout_ms: 0,
            port: 0,
            video_caps: 0,
            audio_channels: 0,
            audio_bits: 0,
            video_codecs: 0,
            preferred_codec: 0,
            client_caps: 0,
            reserved1: 0,
            video_fit: 0,
            reserved0: [0; 3],
            preset_id: ptr::null(),
            preset_name: ptr::null(),
            delivery_profile: 0,
            delivery_flags: 0,
            reserved2: [0; 6],
            profile_id: ptr::null(),
        }
    }
}

/// What the entry points before [`punktfunk_connect_ex7`] imply: stereo, HEVC only.
#[cfg(feature = "quic")]
fn legacy_opts() -> PunktfunkConnectOpts {
    PunktfunkConnectOpts {
        audio_channels: 2,
        video_codecs: PUNKTFUNK_CODEC_HEVC,
        ..Default::default()
    }
}

/// What [`punktfunk_set_session_preset`] last named. Process-wide, so a connect whose opts name
/// no preset reads it once, at entry, into that dial's own parameters.
#[cfg(feature = "quic")]
static SESSION_PRESET: std::sync::Mutex<Option<punktfunk_core::quic::SessionPreset>> =
    std::sync::Mutex::new(None);

/// Name the settings preset the next connect sends: its stable id and display name. The host
/// shows it and hands it to hooks; the stream is unchanged. A null `id` names none. The value
/// outlives the call, so set it before every connect. ABI v38.
///
/// Process-wide: two overlapping dials share it. [`PunktfunkConnectOpts::preset_id`] names a
/// preset for one dial and wins over this.
///
/// # Safety
/// `id` and `name` are null or NUL-terminated C strings, read during this call only.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_set_session_preset(
    id: *const std::os::raw::c_char,
    name: *const std::os::raw::c_char,
) {
    // SAFETY: null or NUL-terminated per the contract above.
    let preset = match unsafe { (opt_cstr(id), opt_cstr(name)) } {
        (Ok(Some(id)), name) => {
            punktfunk_core::quic::SessionPreset::new(id, name.ok().flatten().unwrap_or(""))
        }
        _ => None,
    };
    *lock_recover(&SESSION_PRESET) = preset;
}

/// Minimum `struct_size` [`punktfunk_connect_opts`] accepts. Frozen: when the
/// struct grows this stays put so older callers keep connecting; only the size
/// asserts above move.
#[cfg(all(feature = "quic", target_pointer_width = "64"))]
const CONNECT_OPTS_MIN_SIZE: usize = 96;
#[cfg(all(feature = "quic", target_pointer_width = "32"))]
const CONNECT_OPTS_MIN_SIZE: usize = 68;

/// Connect with every option in one growable [`PunktfunkConnectOpts`]: the `connect_ex*`
/// family's arguments, `video_fit` and the session preset. New options land only in this
/// struct; the `connect_ex*` entry points stay frozen.
///
/// `status_out` (nullable) is written on every path; `observed_sha256_out`
/// (null or 32 bytes) receives the host fingerprint on success.
///
/// # Safety
/// `opts` is null or points to at least `opts->struct_size` readable bytes laid
/// out as its declared [`PunktfunkConnectOpts`]; pointer fields follow
/// [`punktfunk_connect_ex11`]; `observed_sha256_out` is null or valid for 32 bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connect_opts(
    opts: *const PunktfunkConnectOpts,
    observed_sha256_out: *mut u8,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    let set_status = |s: punktfunk_core::error::PunktfunkStatus| {
        // SAFETY: the caller passes `status_out` null or writable for one value.
        unsafe { put(status_out, s as i32) };
    };
    if opts.is_null() {
        set_status(punktfunk_core::error::PunktfunkStatus::NullPointer);
        return std::ptr::null_mut();
    }
    // Size prefix first; a shorter caller gets a zeroed tail, not a misread.
    // SAFETY: `addr_of!` does not form a `&`; the caller may have a different size.
    let declared = unsafe { std::ptr::addr_of!((*opts).struct_size).read_unaligned() } as usize;
    if declared < CONNECT_OPTS_MIN_SIZE {
        set_status(punktfunk_core::error::PunktfunkStatus::InvalidArg);
        return std::ptr::null_mut();
    }
    // Copy the known prefix over zeros so a shorter caller's missing tail stays unspecified.
    let mut o = PunktfunkConnectOpts::default();
    let take = declared.min(std::mem::size_of::<PunktfunkConnectOpts>());
    // SAFETY: `opts` is readable for `declared >= take`; `o` is a local and cannot overlap.
    // Every field is an integer or a raw pointer, so any bytes are a valid value.
    unsafe {
        std::ptr::copy_nonoverlapping(
            opts.cast::<u8>(),
            std::ptr::addr_of_mut!(o).cast::<u8>(),
            take,
        );
    }
    // SAFETY: pointer fields forwarded unchanged; the copy did not deref what they point at.
    unsafe { connect_ex_impl(&o, observed_sha256_out, status_out) }
}

/// Shared body of the connect family: [`connect_params`], then the dial. `status_out` is
/// written on every path.
///
/// # Safety
/// `o`'s pointer fields follow [`punktfunk_connect_opts`]; `observed_sha256_out` is null or
/// valid for 32 bytes; `status_out` is null or writable for one `i32`.
#[cfg(feature = "quic")]
unsafe fn connect_ex_impl(
    o: &PunktfunkConnectOpts,
    observed_sha256_out: *mut u8,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    let set_status = |s: punktfunk_core::error::PunktfunkStatus| {
        // SAFETY: the caller passes `status_out` null or writable for one value.
        unsafe { put(status_out, s as i32) };
    };
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `o`'s pointer fields are the caller's, under this fn's contract.
        let params = match unsafe { connect_params(o) } {
            Ok(p) => p,
            Err(s) => {
                set_status(s);
                return std::ptr::null_mut();
            }
        };
        match punktfunk_core::client::NativeClient::connect(params) {
            Ok(c) => {
                // SAFETY: `observed_sha256_out` is null or writable for 32 bytes (caller contract).
                unsafe { put_sha256(observed_sha256_out, c.host_fingerprint) };
                set_status(punktfunk_core::error::PunktfunkStatus::Ok);
                Box::into_raw(Box::new(PunktfunkConnection {
                    inner: c,
                    last: std::sync::Mutex::new(None),
                    last_audio: std::sync::Mutex::new(None),
                    audio_pcm: std::sync::Mutex::new(AudioPcmState::default()),
                    last_clip: std::sync::Mutex::new(None),
                    last_cursor_shape: std::sync::Mutex::new(None),
                    hud_snap: std::sync::Mutex::new(punktfunk_core::hud::StatsSnapshot::default()),
                }))
            }
            Err(e) => {
                set_status(e.status());
                std::ptr::null_mut()
            }
        }
    }));
    r.unwrap_or_else(|_| {
        set_status(punktfunk_core::error::PunktfunkStatus::Panic);
        std::ptr::null_mut()
    })
}

/// The dial `o` asks for. A null or non-UTF-8 `host` and half an identity are `InvalidArg`;
/// a bad launch id, device name or preset degrades to none (the OS name for the device).
///
/// # Safety
/// Each pointer field of `o` is null or valid as [`punktfunk_connect_opts`] documents.
#[cfg(feature = "quic")]
unsafe fn connect_params(
    o: &PunktfunkConnectOpts,
) -> Result<punktfunk_core::client::ConnectParams, PunktfunkStatus> {
    // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
    let Ok(Some(host)) = (unsafe { opt_cstr(o.host) }) else {
        return Err(PunktfunkStatus::InvalidArg);
    };
    // SAFETY: as above.
    let launch = match unsafe { opt_cstr(o.launch_id) } {
        Ok(Some(s)) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    };
    // Truncate on a character boundary; empty is the OS default.
    // SAFETY: as above.
    let name = match unsafe { opt_cstr(o.device_name) } {
        Ok(Some(s)) if !s.trim().is_empty() => clamp_device_name(s.trim()),
        _ => punktfunk_core::client::device_name(),
    };
    // Unrecognized = Auto must hold for the full u32 domain: `as u8` would wrap
    // 0x101 into a concrete choice before `from_u8`'s fallback could apply.
    let compositor = u8::try_from(o.compositor)
        .map(punktfunk_core::config::CompositorPref::from_u8)
        .unwrap_or_default();
    let gamepad = u8::try_from(o.gamepad)
        .map(punktfunk_core::config::GamepadPref::from_u8)
        .unwrap_or_default();
    let pin = if o.pin_sha256.is_null() {
        None
    } else {
        let mut p = [0u8; 32];
        // SAFETY: a non-null pin is valid for 32 bytes (caller contract); copied out here.
        p.copy_from_slice(unsafe { std::slice::from_raw_parts(o.pin_sha256, 32) });
        Some(p)
    };
    // SAFETY: as above.
    let identity = match unsafe { (opt_cstr(o.client_cert_pem), opt_cstr(o.client_key_pem)) } {
        (Ok(Some(c)), Ok(Some(k))) => Some((c.to_string(), k.to_string())),
        (Ok(None), Ok(None)) => None,
        // Half an identity or bad UTF-8: fail closed.
        _ => return Err(PunktfunkStatus::InvalidArg),
    };
    // SAFETY: as above.
    let preset = match unsafe { opt_cstr(o.preset_id) } {
        Ok(None) => lock_recover(&SESSION_PRESET).clone(),
        Ok(Some(id)) => {
            // SAFETY: as above.
            let name = unsafe { opt_cstr(o.preset_name) }.ok().flatten();
            punktfunk_core::quic::SessionPreset::new(id, name.unwrap_or(""))
        }
        Err(()) => None,
    };
    // An id the host can't hold is no ask: the host then picks, as it does for null.
    // SAFETY: as above.
    let profile = match unsafe { opt_cstr(o.profile_id) } {
        Ok(Some(id)) if !id.is_empty() && id.len() <= PUNKTFUNK_PROFILE_ID_MAX => {
            Some(id.to_string())
        }
        _ => None,
    };
    let mode = punktfunk_core::config::Mode {
        width: o.width,
        height: o.height,
        refresh_hz: o.refresh_hz,
    };
    Ok(punktfunk_core::client::ConnectParams {
        compositor,
        gamepad,
        bitrate_kbps: o.bitrate_kbps,
        video_caps: o.video_caps,
        audio_channels: punktfunk_core::audio::normalize_channels(o.audio_channels),
        // Unvalidated on purpose: a bad rate is the host's to decline, not a failed connect.
        audio_rate_hz: o.audio_rate_hz,
        audio_bits: o.audio_bits,
        video_fit: punktfunk_core::video_fit::VideoFit::from_wire(o.video_fit),
        video_codecs: o.video_codecs,
        preferred_codec: o.preferred_codec,
        // CLIENT_CAP_CURSOR: host stops compositing; only if the embedder draws the cursor.
        client_caps: o.client_caps,
        launch,
        name: Some(name),
        pin,
        identity,
        preset,
        profile,
        delivery: (o.delivery_profile != 0 || o.delivery_flags != 0).then_some(
            punktfunk_core::quic::DeliveryAsk {
                profile: o.delivery_profile,
                flags: o.delivery_flags,
            },
        ),
        // The rest stays default: Legacy coupling (embedders decode what the host answers),
        // no display volume, whole AUs (`PunktfunkFrame` cannot tell a part), no abort.
        ..punktfunk_core::client::ConnectParams::new(
            host,
            o.port,
            mode,
            std::time::Duration::from_millis(u64::from(o.timeout_ms)),
        )
    })
}

/// Generate a persistent client identity: self-signed certificate + private key,
/// both PEM, NUL-terminated, written into the caller's buffers. Generate once,
/// store both, pass them to [`punktfunk_pair`] and every [`punktfunk_connect`].
/// Hosts recognize this client by the certificate fingerprint. 4096-byte buffers
/// are ample.
///
/// # Safety
/// `cert_pem_out` is writable for `cert_cap` bytes; `key_pem_out` for `key_cap`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_generate_identity(
    cert_pem_out: *mut std::os::raw::c_char,
    cert_cap: usize,
    key_pem_out: *mut std::os::raw::c_char,
    key_cap: usize,
) -> PunktfunkStatus {
    guard(|| {
        if cert_pem_out.is_null() || key_pem_out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let (cert, key) = match punktfunk_core::quic::endpoint::generate_identity() {
            Ok(t) => t,
            Err(_) => return PunktfunkStatus::Io,
        };
        // Both fit or neither is written.
        if cert.len() + 1 > cert_cap || key.len() + 1 > key_cap {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: each buffer is writable for its `cap` bytes, per this function's contract.
        unsafe {
            write_cstr(cert_pem_out, cert_cap, &cert);
            write_cstr(key_pem_out, key_cap, &key);
        }
        PunktfunkStatus::Ok
    })
}

/// QUIC reachability probe, mDNS-independent. `Ok` if something answered, `Timeout`
/// otherwise. Blocks up to `timeout_ms`; off the UI thread.
///
/// The handshake is unpinned, so `Ok` says an address is occupied, not that it is YOUR
/// host: pass `observed_sha256_out` and compare it against the record's pin. A stranger
/// who inherits a sleeping host's lease answers too, and counting that as the host lights
/// the pip and shuts the wake gate against the machine that needs waking.
///
/// # Safety
/// `host` is a NUL-terminated UTF-8 string; `observed_sha256_out` is null, or writable
/// for 32 bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_probe(
    host: *const std::os::raw::c_char,
    port: u16,
    timeout_ms: u32,
    observed_sha256_out: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: pointers are caller-supplied and null-checked on this path.
        let Ok(Some(host)) = (unsafe { opt_cstr(host) }) else {
            return PunktfunkStatus::NullPointer;
        };
        match punktfunk_core::client::NativeClient::probe_identity(
            host,
            port,
            std::time::Duration::from_millis(timeout_ms as u64),
        ) {
            Some(fp) => {
                // SAFETY: `observed_sha256_out` is null or writable for 32 bytes (caller contract).
                unsafe { put_sha256(observed_sha256_out, fp) };
                PunktfunkStatus::Ok
            }
            None => PunktfunkStatus::Timeout,
        }
    })
}

/// PIN pairing: the host displays a PIN; pass it here. On success the host has
/// stored this client's identity and the verified host fingerprint is written to
/// `host_sha256_out` (32 bytes) — persist it as `pin_sha256` for
/// [`punktfunk_connect`]. [`PunktfunkStatus::Crypto`] for a wrong PIN.
///
/// # Safety
/// `host`/`client_cert_pem`/`client_key_pem`/`pin`/`name` are NUL-terminated UTF-8;
/// `host_sha256_out` is writable for 32 bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_pair(
    host: *const std::os::raw::c_char,
    port: u16,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    pin: *const std::os::raw::c_char,
    name: *const std::os::raw::c_char,
    host_sha256_out: *mut u8,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        let (Ok(Some(host)), Ok(Some(cert)), Ok(Some(key)), Ok(Some(pin)), Ok(Some(name))) = (
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            unsafe { opt_cstr(host) },
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            unsafe { opt_cstr(client_cert_pem) },
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            unsafe { opt_cstr(client_key_pem) },
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            unsafe { opt_cstr(pin) },
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            unsafe { opt_cstr(name) },
        ) else {
            return PunktfunkStatus::NullPointer;
        };
        if host_sha256_out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match punktfunk_core::client::NativeClient::pair(
            host,
            port,
            (cert, key),
            pin,
            name,
            std::time::Duration::from_millis(timeout_ms as u64),
        ) {
            Ok(fp) => {
                // SAFETY: `host_sha256_out` is non-null here and writable for 32 bytes.
                unsafe { put_sha256(host_sha256_out, fp) };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

#[cfg(all(test, feature = "quic"))]
mod tests {
    use super::*;

    /// `video_fit` and the preset reach the dial. A null `preset_id` takes what
    /// `punktfunk_set_session_preset` named; a named one wins over it.
    #[test]
    fn connect_opts_carry_fit_and_preset() {
        let mut o = PunktfunkConnectOpts {
            host: c"127.0.0.1".as_ptr(),
            video_fit: PUNKTFUNK_VIDEO_FIT_CROP,
            preset_id: c"dock-1".as_ptr(),
            preset_name: c"Docked".as_ptr(),
            ..Default::default()
        };
        // SAFETY: C-string literals outlive the call; `name` null names none.
        unsafe { punktfunk_set_session_preset(c"old".as_ptr(), std::ptr::null()) };
        // SAFETY: every pointer field is null or a live C-string literal.
        let p = unsafe { connect_params(&o) }.unwrap();
        assert_eq!(p.video_fit, punktfunk_core::video_fit::VideoFit::Crop);
        assert_eq!(
            p.preset,
            punktfunk_core::quic::SessionPreset::new("dock-1", "Docked")
        );

        o.preset_id = std::ptr::null();
        // SAFETY: as above.
        let p = unsafe { connect_params(&o) }.unwrap();
        assert_eq!(
            p.preset,
            punktfunk_core::quic::SessionPreset::new("old", "")
        );
        // SAFETY: null `id` clears the fallback.
        unsafe { punktfunk_set_session_preset(std::ptr::null(), std::ptr::null()) };
    }

    /// `profile_id` reaches the dial; one longer than the wire carries is no ask.
    #[test]
    fn connect_opts_carry_the_profile() {
        let long = format!("{}\0", "9".repeat(PUNKTFUNK_PROFILE_ID_MAX + 1));
        let mut o = PunktfunkConnectOpts {
            host: c"127.0.0.1".as_ptr(),
            profile_id: c"9a3f1c2b7e40".as_ptr(),
            ..Default::default()
        };
        // SAFETY: every pointer field is null or a live C-string literal.
        let p = unsafe { connect_params(&o) }.unwrap();
        assert_eq!(p.profile.as_deref(), Some("9a3f1c2b7e40"));
        o.profile_id = long.as_ptr().cast();
        // SAFETY: `long` is NUL-terminated and outlives the call.
        assert_eq!(unsafe { connect_params(&o) }.unwrap().profile, None);
        o.profile_id = std::ptr::null();
        // SAFETY: as above.
        assert_eq!(unsafe { connect_params(&o) }.unwrap().profile, None);
    }

    /// Size-prefix guard: null/undersized is a status, not a read.
    #[test]
    fn connect_opts_guards_size_prefix() {
        let mut status = 0i32;
        // SAFETY: null `opts` is the documented reported-not-UB case.
        let c =
            unsafe { punktfunk_connect_opts(std::ptr::null(), std::ptr::null_mut(), &mut status) };
        assert!(c.is_null());
        assert_eq!(status, PunktfunkStatus::NullPointer as i32);

        // SAFETY: an all-zero struct is a valid value (null pointers, zero scalars).
        let mut o: PunktfunkConnectOpts = unsafe { std::mem::zeroed() };
        o.struct_size = 4; // smaller than CONNECT_OPTS_MIN_SIZE
                           // SAFETY: `o` outlives the call; out-params are null or a live local.
        let c = unsafe { punktfunk_connect_opts(&o, std::ptr::null_mut(), &mut status) };
        assert!(c.is_null());
        assert_eq!(status, PunktfunkStatus::InvalidArg as i32);

        o.struct_size = std::mem::size_of::<PunktfunkConnectOpts>() as u32;
        // SAFETY: as above; the null `host` field is the documented InvalidArg path.
        let c = unsafe { punktfunk_connect_opts(&o, std::ptr::null_mut(), &mut status) };
        assert!(c.is_null());
        assert_eq!(status, PunktfunkStatus::InvalidArg as i32);
    }

    #[test]
    fn identity_rejects_undersized_buffers_without_writing() {
        let mut cert: [std::os::raw::c_char; 2] = [0x5a; 2];
        let mut key: [std::os::raw::c_char; 2] = [0x5a; 2];

        // SAFETY: each array is writable for the one-byte capacity reported to the core.
        let status =
            unsafe { punktfunk_generate_identity(cert.as_mut_ptr(), 1, key.as_mut_ptr(), 1) };

        assert_eq!(status, PunktfunkStatus::InvalidArg);
        assert_eq!(cert, [0x5a; 2]);
        assert_eq!(key, [0x5a; 2]);
    }

    /// Truncation lands on a character boundary; `s[..HELLO_NAME_MAX]` would panic mid-scalar.
    #[test]
    fn device_name_truncates_on_a_character_boundary() {
        let max = punktfunk_core::quic::HELLO_NAME_MAX;
        assert_eq!(clamp_device_name("Enrico's iPad"), "Enrico's iPad");

        // Straddling: 2-byte characters over an odd-length prefix, so the cap lands mid-scalar.
        let straddle = format!("{}{}", "x".repeat(max - 1), "ü".repeat(4));
        let cut = clamp_device_name(&straddle);
        assert!(cut.len() <= max, "{} bytes exceeds the cap", cut.len());
        assert_eq!(
            cut,
            "x".repeat(max - 1),
            "must drop the whole ü, not half of it"
        );

        // A name whose first character already exceeds the cap has nothing to keep —
        // `unwrap_or(0)` must yield "" rather than panicking on an empty iterator.
        assert_eq!(clamp_device_name(&"あ".repeat(max)), "あ".repeat(max / 3));
    }
}
