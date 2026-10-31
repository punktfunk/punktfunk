//! Native `punktfunk/1` Hello → Welcome → Start negotiation.
//!
//! Pairing stays in `serve_session`: the delegated-approval wait must outlive this handshake
//! timeout and must release the session permit. This module then admits the client, negotiates
//! encode / audio / cursor, binds the data-plane UDP socket, and returns the values
//! `serve_session` needs to stand the session up. On `punktfunk/2` a client can instead be
//! sent to another host of the box before any `Welcome` ([`redirect`]).
//!
//! Evidence: `design/hi-res-audio.md`, `design/remote-desktop-sweep.md`.

use super::*;
use crate::native::bitrate::{
    audio_reserved_kbps, resolve_bitrate_kbps_for, session_pyrowave_bpp, EncDerive,
};
use punktfunk_core::abr::budget::FEC_ADAPTIVE_START;

/// Encode tier and `0xD2` redundancy, budgeted against this session's video kbps.
///
/// [`plan_audio_budget`](punktfunk_core::audio::plan_audio_budget) may lower the operator's
/// `audio.quality` / `audio.redundancy` request; it never raises them. Audio rides QUIC
/// datagrams outside ABR, so this budget is the only cap.
///
/// Redundancy is decided once here, not when the link is losing packets: flipping `0xD2`
/// mid-session changes the wire tag and the decoder cannot re-derive the plane from a
/// datagram. `wants_redundancy` is the caller's answer — client cap and operator at
/// handshake, then the granted `HOST_CAP_AUDIO_RED` bit — so the audio thread re-derives
/// the same rung.
pub(super) fn audio_budget(
    wants_redundancy: bool,
    video_kbps: u32,
    channels: u8,
    layout: punktfunk_core::audio::AudioLayout,
) -> punktfunk_core::audio::AudioBudget {
    let configured = pf_host_config::config().audio_quality.as_deref();
    let requested = match configured {
        None => punktfunk_core::audio::AudioTier::default(),
        Some(s) => punktfunk_core::audio::AudioTier::parse(s).unwrap_or_else(|| {
            // Once: this runs per session; a typo in host.env must not warn on every connect.
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                tracing::warn!(
                    value = %s,
                    "audio.quality (PUNKTFUNK_AUDIO_QUALITY) is not one of low/standard/high — \
                     using the default"
                );
            });
            punktfunk_core::audio::AudioTier::default()
        }),
    };
    punktfunk_core::audio::plan_audio_budget(
        video_kbps,
        channels,
        layout,
        requested,
        wants_redundancy,
    )
}

pub(super) fn redundancy_offered(client_caps: u8) -> bool {
    client_caps & punktfunk_core::quic::CLIENT_CAP_AUDIO_RED != 0
        && pf_host_config::config().audio_redundancy.unwrap_or(true)
}

/// Codec / rate / bits / frame length the `Welcome` states and the audio thread is built from.
/// Produced together by [`resolve_bitstream_plane`] or [`resolve_audio_plane`] so the two
/// cannot disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AudioPlane {
    /// [`AUDIO_CODEC_OPUS`](punktfunk_core::quic::AUDIO_CODEC_OPUS),
    /// [`AUDIO_CODEC_PCM`](punktfunk_core::quic::AUDIO_CODEC_PCM), or
    /// [`AUDIO_CODEC_AC3`](punktfunk_core::quic::AUDIO_CODEC_AC3) /
    /// [`AUDIO_CODEC_EAC3`](punktfunk_core::quic::AUDIO_CODEC_EAC3) on `0xD4`.
    pub codec: u8,
    pub rate_hz: u32,
    pub bits: u8,
    /// Frame duration in µs on the PCM plane, the access unit's label on `0xD4`; `0` on Opus
    /// (`0xC9` is a fixed 5 ms).
    pub frame_us: u16,
    /// Surround coupling the encoder uses: the client's ask when this host knows it, else legacy.
    pub layout: punktfunk_core::audio::AudioLayout,
}

impl AudioPlane {
    /// Opus 48 kHz 16-bit — the failed-gate answer and the pre-hi-res wire form.
    fn opus() -> AudioPlane {
        AudioPlane {
            codec: punktfunk_core::quic::AUDIO_CODEC_OPUS,
            rate_hz: punktfunk_core::audio::SAMPLE_RATE_HZ,
            bits: punktfunk_core::audio::pcm::BITS_16,
            frame_us: 0,
            layout: punktfunk_core::audio::AudioLayout::Legacy,
        }
    }

    pub fn is_pcm(self) -> bool {
        self.codec == punktfunk_core::quic::AUDIO_CODEC_PCM
    }

    /// AC-3 / E-AC-3 on `0xD4`.
    pub fn is_bitstream(self) -> bool {
        punktfunk_core::audio::bitstream::is_bitstream(self.codec)
    }

    /// What a lossless plane falls back to when its capture loses the promised rate: 48 kHz
    /// Opus in the same coupling, which the capturer resamples to.
    pub(super) fn opus_fallback(self) -> AudioPlane {
        AudioPlane {
            layout: self.layout,
            ..AudioPlane::opus()
        }
    }

    /// This plane as the desktop stream's `AudioConfig` from `first_seq` on.
    pub(super) fn config(
        self,
        channels: u8,
        epoch: u8,
        first_seq: u32,
        redundant: bool,
    ) -> punktfunk_core::quic::v2::msg::AudioConfig {
        punktfunk_core::quic::v2::msg::AudioConfig {
            stream: 0,
            epoch,
            codec: self.codec,
            rate_hz: self.rate_hz,
            bits: self.bits,
            channels,
            layout: self.layout.wire(),
            frame_us: self.frame_us,
            redundant,
            first_seq,
        }
    }

    /// Read the plane back off the sent [`Welcome`]. Recomputing it would let encoder and client drift.
    pub(super) fn from_welcome(w: &Welcome) -> AudioPlane {
        AudioPlane {
            codec: w.audio_codec,
            rate_hz: w.audio_rate_hz,
            bits: w.audio_bits,
            frame_us: w.audio_frame_us,
            layout: punktfunk_core::audio::AudioLayout::from_wire(w.audio_layout)
                .unwrap_or_default(),
        }
    }
}

/// Max share of the session video bitrate the hi-res or passthrough plane may take.
///
/// Audio is outside ABR, so this is not "video adapts around 25 %". Separate from
/// [`plan_audio_budget`](punktfunk_core::audio::plan_audio_budget)'s 5 % Opus ladder: hi-res
/// is opt-in, never a new rung. Stereo 48/16 (1 536 kbps) needs ≥ 6.1 Mbps of video;
/// 176.4/24 7.1 needs ≥ 33.9. AC-3 5.1 (448 kbps) needs ≥ 1.8; E-AC-3 7.1 (1 024) ≥ 4.1.
const HIRES_MAX_VIDEO_SHARE_PCT: u32 = 25;

/// Blocks in one E-AC-3 access unit: 16 ms. AC-3 is always 6.
const EAC3_BLOCKS: u32 = 3;

/// The `0xD4` passthrough plane and the channel count it codes, for a client whose sink plays
/// AC-3 / E-AC-3 through (`asked` is [`Hello::audio_bitstreams`]). `None` when it asked for
/// neither or a gate declines; [`resolve_audio_plane`] then runs as if it had not asked.
///
/// E-AC-3 when offered, else AC-3, which has no 7.1 and codes it as 5.1. The rate is fixed per
/// layout ([`bitrate_kbps`](punktfunk_core::audio::bitstream::bitrate_kbps)) and must fit the
/// [`HIRES_MAX_VIDEO_SHARE_PCT`] share. An access unit fragments over datagrams, so any
/// datagram size carries it.
///
/// Pure, like [`resolve_audio_plane`].
pub(super) fn resolve_bitstream_plane(
    asked: u8,
    channels: u8,
    video_kbps: u32,
    max_datagram: Option<usize>,
) -> Option<(AudioPlane, u8)> {
    use punktfunk_core::audio::bitstream;
    use punktfunk_core::quic::{AUDIO_CODEC_AC3, AUDIO_CODEC_EAC3};
    let (codec, name) = if asked & bitstream::AUDIO_BITSTREAM_EAC3 != 0 {
        (AUDIO_CODEC_EAC3, "E-AC-3")
    } else if asked & bitstream::AUDIO_BITSTREAM_AC3 != 0 {
        (AUDIO_CODEC_AC3, "AC-3")
    } else {
        // Not logged: every ordinary client asks for neither.
        return None;
    };
    let mut coded = punktfunk_core::audio::normalize_channels(channels);
    if codec == AUDIO_CODEC_AC3 && coded == 8 {
        tracing::info!("AC-3 has no 7.1 — the passthrough plane codes 5.1");
        coded = 6;
    }
    let kbps = bitstream::bitrate_kbps(codec, coded);
    if kbps == 0 {
        // A layout the table has no rate for; none survives the fold today.
        return None;
    }
    if max_datagram.is_none() {
        tracing::info!(
            codec = name,
            "audio passthrough needs QUIC datagrams and this connection reports none available \
             — declining it"
        );
        return None;
    }
    let allowance = video_kbps.saturating_mul(HIRES_MAX_VIDEO_SHARE_PCT) / 100;
    if kbps > allowance {
        tracing::info!(
            codec = name,
            kbps,
            video_kbps,
            allowance_kbps = allowance,
            max_share_pct = HIRES_MAX_VIDEO_SHARE_PCT,
            "audio passthrough would take more of this session's bitrate than it can spare — \
             audio rides outside the ABR loop, so its cost comes off the top; declining it"
        );
        return None;
    }
    let frame_us = bitstream::frame_us(codec, EAC3_BLOCKS);
    tracing::info!(
        codec = name,
        channels = coded,
        kbps,
        frame_us,
        video_kbps,
        "audio passthrough resolved — the session runs the 0xD4 plane"
    );
    Some((
        AudioPlane {
            codec,
            rate_hz: punktfunk_core::audio::SAMPLE_RATE_HZ,
            bits: punktfunk_core::audio::pcm::BITS_16,
            frame_us,
            layout: punktfunk_core::audio::AudioLayout::Legacy,
        },
        coded,
    ))
}

/// Resolve the session audio plane. Returns [`AudioPlane::opus`] unless every gate holds.
///
/// Not a downgrade ladder: a 96/24 ask on a 48/24 link is declined, not quietly cheapened.
/// The client opens its device from the `Welcome`, so a different quality would be a product
/// choice this pass does not make.
///
/// Capture is asked whether it can actually deliver the rate: both backends accept a rate they
/// resample without error. Channel count is decided only by [`pcm::frame_us_for`] — there is
/// no stereo-only rule. `max_datagram` is `None` when the peer has no datagrams, in which case
/// there is no audio plane.
///
/// Pure: operator policy and the capture probe are arguments, not process state.
#[allow(clippy::too_many_arguments)] // one parameter per gate; a struct would only rename them
pub(super) fn resolve_audio_plane(
    client_asked: bool,
    operator_allows: bool,
    requested_rate_hz: u32,
    requested_bits: u8,
    channels: u8,
    capture_rate: crate::audio::CaptureRate,
    video_kbps: u32,
    max_datagram: Option<usize>,
) -> AudioPlane {
    use punktfunk_core::audio::pcm;
    // Not logged: every ordinary client skips this feature; the line would be noise.
    if !client_asked {
        return AudioPlane::opus();
    }
    if !operator_allows {
        tracing::info!(
            // Reaching here means PUNKTFUNK_AUDIO_HIRES=0. Name the variable so the operator finds it.
            "hi-res audio requested by the client but it is disabled on this host by \
             PUNKTFUNK_AUDIO_HIRES=0 — the session uses Opus 48 kHz (remove that line, or set it \
             to 1, to allow the lossless plane; it costs 1.4–8.5 Mbps in stereo and up to 33.9 in \
             7.1, off the top of the link and outside the ABR loop)"
        );
        return AudioPlane::opus();
    }
    // No channel-count test here. `pcm::frame_us_for` is channel-aware; an early `!= 2` would override it.
    if !pcm::depth_is_supported(requested_bits) || !pcm::rate_is_supported(requested_rate_hz) {
        tracing::info!(
            requested_rate_hz,
            requested_bits,
            "hi-res audio was requested at a format this host does not carry (44 100 / 48 000 / \
             88 200 / 96 000 / 176 400 Hz, 16 or 24-bit) — the session uses Opus 48 kHz"
        );
        return AudioPlane::opus();
    }
    // Before Welcome: after the client opens its device at the promised rate the only move is silence.
    if !capture_rate.can_deliver(requested_rate_hz) {
        tracing::info!(
            requested_rate_hz,
            requested_bits,
            ?capture_rate,
            "hi-res audio was requested but this host's capture path cannot honestly deliver \
             that rate — the session uses Opus 48 kHz. On Windows the endpoint's own engine rate \
             is authoritative (autoconvert would silently hand us an upsampled copy), so set the \
             rate in that device's Windows properties; on Linux the default stream-sink mode \
             delivers any supported rate, while PUNKTFUNK_STREAM_SINK=0 can only offer the rate \
             the monitored sink itself runs at — and declines outright when that sink is idle or \
             cannot be read"
        );
        return AudioPlane::opus();
    }
    let cost_kbps = pcm::bitrate_kbps(requested_rate_hz, requested_bits, channels);
    let allowance = video_kbps.saturating_mul(HIRES_MAX_VIDEO_SHARE_PCT) / 100;
    if cost_kbps > allowance {
        tracing::info!(
            requested_rate_hz,
            requested_bits,
            cost_kbps,
            video_kbps,
            allowance_kbps = allowance,
            max_share_pct = HIRES_MAX_VIDEO_SHARE_PCT,
            "hi-res audio would take more of this session's bitrate than it can spare — audio \
             rides outside the ABR loop, so its cost comes off the top and ABR can neither see \
             nor reclaim it; the session uses Opus 48 kHz"
        );
        return AudioPlane::opus();
    }
    let Some(max_datagram) = max_datagram else {
        tracing::info!(
            "hi-res audio needs QUIC datagrams and this connection reports none available — the \
             session uses Opus 48 kHz"
        );
        return AudioPlane::opus();
    };
    // Channel count is only a multiplier for `frame_us_for`; `None` is the decline.
    // 44.1 kHz fits the same rung or a longer one than 48 kHz (fewer samples per ms).
    // Packet rate is not policed here — the affordability gate above is in bits.
    let Some(frame_us) =
        pcm::frame_us_for(requested_rate_hz, requested_bits, channels, max_datagram)
    else {
        tracing::info!(
            requested_rate_hz,
            requested_bits,
            channels,
            max_datagram,
            "no hi-res frame duration fits this connection's datagram size — the session uses \
             Opus 48 kHz. This plane is never fragmented, so a frame that would not fit one \
             datagram is not sent at all; surround and the rates above 96 kHz are what reach \
             this, and a jumbo path (PUNKTFUNK_WIRE_MTU) is what would carry them"
        );
        return AudioPlane::opus();
    };
    tracing::info!(
        rate_hz = requested_rate_hz,
        bits = requested_bits,
        frame_us,
        cost_kbps,
        video_kbps,
        max_datagram,
        // `Declared` vs `Engine(96000)` are different grounds for the same yes.
        ?capture_rate,
        "hi-res audio resolved — the session runs the lossless 0xD3 PCM plane"
    );
    AudioPlane {
        codec: punktfunk_core::quic::AUDIO_CODEC_PCM,
        rate_hz: requested_rate_hz,
        bits: requested_bits,
        frame_us: frame_us as u16,
        layout: punktfunk_core::audio::AudioLayout::Legacy,
    }
}

/// Out-of-band cursor: client asked, capture can deliver metadata, `encoder` can blend.
///
/// Welcome `HOST_CAP_CURSOR` is computed from this; session wiring reads that bit back.
/// Denied, the host still composites wherever the backend can blend.
pub(super) fn cursor_forward(
    client_caps: u8,
    compositor: Option<crate::vdisplay::Compositor>,
    codec: crate::encode::Codec,
    bit_depth: u8,
    hdr: bool,
    joined: bool,
    encoder: &crate::encode::Resolved,
) -> bool {
    if client_caps & punktfunk_core::quic::CLIENT_CAP_CURSOR == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        // Same CUDA prediction `SessionPlan` makes; it picks the backend.
        let cuda_planned = !encoder.zero_copy_is_vaapi() && crate::zerocopy::enabled();
        let _ = joined;
        compositor.is_some_and(|c| c != crate::vdisplay::Compositor::Gamescope)
            && encoder.cursor_blend_capable(codec, cuda_planned, bit_depth == 10, hdr)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Windows: the v5 IddCx hardware-cursor channel. Without it DWM paints the pointer
        // into the IDD frame and a second copy doubles it. The encoder is not consulted: the
        // IDD capturer composites on the capture-mouse flip; no Windows encode backend blends.
        // A mirrored monitor's pointer, and a shared display's, is in the picture the capture
        // worker takes.
        let _ = (compositor, codec, bit_depth, hdr, encoder);
        !joined && !crate::session_plan::mirrored() && crate::windows::idd::hw_cursor_capable()
    }
}

/// Whether the session's compositor reads the uinput tablet pen batches land on. gamescope
/// reads none, so its clients send the pen as touch.
fn pen_reaches(compositor: Option<crate::vdisplay::Compositor>) -> bool {
    compositor != Some(crate::vdisplay::Compositor::Gamescope)
}

/// [`Hello::preferred_codec`] for the negotiation line. `none` is auto; a byte this build
/// does not know is `unknown` rather than blank.
fn codec_pref_label(preferred: u8) -> &'static str {
    match preferred {
        0 => "none",
        p => match punktfunk_core::hud::codec_label(p) {
            "" => "unknown",
            label => label,
        },
    }
}

/// The line under a negotiation whose preference lost, naming the side that lacks the codec.
/// `preferred` and `picked` are [`punktfunk_core::hud::codec_label`]s.
fn codec_miss_note(miss: punktfunk_core::quic::CodecMiss, preferred: &str, picked: &str) -> String {
    use punktfunk_core::quic::CodecMiss;
    match miss {
        CodecMiss::Client => format!(
            "client prefers {preferred} but does not advertise it (no decoder) — \
             {picked} picked from the shared set"
        ),
        CodecMiss::Host => format!(
            "client prefers {preferred} but this host cannot encode it — \
             {picked} picked from the shared set"
        ),
        CodecMiss::Both => format!(
            "neither the client nor this host does {preferred} — \
             {picked} picked from the shared set"
        ),
    }
}

/// What [`negotiate`] settled, for the session runner.
pub(super) struct Negotiated {
    pub(super) hello: Hello,
    pub(super) welcome: Welcome,
    /// What the client calls itself; `None` from one that sent no label. Log only: two dialers
    /// from one device are told apart by that line.
    pub(super) client_label: Option<String>,
    /// The settings preset the client dialled with.
    pub(super) preset: Option<crate::events::PresetRef>,
    /// The ABR wire features this client reads (`0` = none).
    pub(super) abr_features: u8,
    /// The client's end of the path; the default from a client that said nothing.
    pub(super) client_link: punktfunk_core::quic::LinkFacts,
    /// A diagnostic session.
    pub(super) probe_only: bool,
    /// PyroWave's bits per pixel for this session: the client's quality, else the host's.
    pub(super) pyrowave_bpp: f64,
    /// H.264/HEVC slices per frame ([`crate::session_plan::client_slices`]).
    pub(super) slices: u32,
    /// This host's end of the path, for every `StreamConfig`.
    pub(super) host_link: punktfunk_core::quic::HostLink,
    pub(super) compositor: Option<crate::vdisplay::Compositor>,
    /// Gamescope sub-mode as a value, not process env — a concurrent connect would overwrite env.
    pub(super) gamescope_route: Option<crate::vdisplay::GamescopeRoute>,
    pub(super) prep: Option<super::stream::PrepHandle>,
    /// Admitted by `mode_conflict: join`: the owner's display, which this session shares, and
    /// the size this client asked for.
    pub(super) joined: Option<(crate::vdisplay::admission::LiveDisplay, (u32, u32))>,
    /// This connect's row in the admission table, until the planes register it. Dropped with
    /// the handshake when the session never gets that far.
    pub(super) reservation: std::sync::Mutex<Option<crate::vdisplay::admission::Reservation>>,
    /// Native feature bits in force ([`ServerHello::features`]).
    pub(super) features: punktfunk_core::quic::v2::features::FeatureSet,
    /// The profile this session plays as; `ServerHello` echoed its id.
    pub(super) resolved: crate::profiles::Resolved,
    /// The encode resolution the Welcome was negotiated against, kept for the session.
    pub(super) encoder: Arc<crate::encode::Resolved>,
}

/// How long the host holds the connection after a `Redirect` for the client to read it and
/// leave. A close first could outrun the frame.
const REDIRECT_LINGER: std::time::Duration = std::time::Duration::from_secs(2);

/// Send the client to another host of this box instead of a `ServerHello`: the frame, then the
/// client's close, then ours. Placement by profile is the caller.
pub(crate) async fn redirect(
    conn: &super::link::SessionLink,
    send: &mut super::link::CtlSend,
    to: &punktfunk_core::quic::v2::msg::Redirect,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    punktfunk_core::quic::v2::io::send(send, to).await?;
    send.flush().await?;
    let _ = tokio::time::timeout(REDIRECT_LINGER, conn.closed()).await;
    conn.close(punktfunk_core::quic::QUIT_CLOSE_CODE, b"redirected");
    Ok(())
}

/// What the handshake starts from: the admitted connection, its first `ClientHello`, and what
/// admission resolved. The negotiate phase builds it; [`negotiate`] takes it and its steps
/// borrow it.
pub(super) struct Negotiation<'a> {
    pub(super) conn: &'a super::link::SessionLink,
    /// The already-read `ClientHello`.
    pub(super) first: &'a ClientHello,
    pub(super) source: Punktfunk1Source,
    pub(super) frames: u32,
    /// `welcome` / `start` stamps; Welcome-time display prep threads this into pipeline-build.
    pub(super) bringup: &'a Arc<crate::bringup::Trace>,
    /// The session's flags, so Welcome-time display prep sees a vanished client: `stop` aborts
    /// a build retry, `quit` rides into the display lease.
    pub(super) quit: &'a Arc<AtomicBool>,
    pub(super) stop: &'a Arc<AtomicBool>,
    /// The scope's [`SessionScope::prep_done`]: teardown waits for a prep started here.
    pub(super) prep_done: std::sync::mpsc::SyncSender<()>,
    /// Effective grant mask and seconds until expiry (`0` = permanent), resolved at admission.
    pub(super) grants: u32,
    pub(super) expires_in_secs: u32,
    pub(super) resolved: crate::profiles::Resolved,
    pub(super) knobs: SessionKnobs,
}

/// What the handshake settled before the Welcome states it.
struct Terms {
    /// The client's Hello at the mode it gets: capped, then admitted.
    hello: Hello,
    codec_bit: u8,
    codec: crate::encode::Codec,
    joined: Option<(crate::vdisplay::admission::LiveDisplay, (u32, u32))>,
    compositor: Option<crate::vdisplay::Compositor>,
    gamescope_route: Option<crate::vdisplay::GamescopeRoute>,
    encoder: Arc<crate::encode::Resolved>,
    gamepad: GamepadPref,
    audio_channels: u8,
    bit_depth: u8,
    hdr: bool,
    chroma: crate::encode::ChromaFormat,
    /// The colours the client asked to get at studio range.
    studio_asked: crate::session_plan::StudioAsk,
    pyrowave_bpp: f64,
    slices: u32,
    bitrate_kbps: u32,
    shard_payload: usize,
    chacha: bool,
    audio_plane: AudioPlane,
}

/// `ClientHello` → `ServerHello` → `Ready`. Borrows the control streams; the caller keeps them
/// for mid-stream renegotiation.
pub(super) async fn negotiate(
    n: Negotiation<'_>,
    send: &mut super::link::CtlSend,
    recv: &mut super::link::CtlReader,
) -> Result<Negotiated> {
    let mut hello = n.first.hello.clone();
    // The encoder this handshake advertises and the session keeps; `resolved` is the profile.
    let encoder = crate::encode::resolve();
    // Pairing ran before this future: a client here is paired, or the host is `--open`.
    let (codec_bit, codec) = pick_codec(&hello, &encoder)?;
    settle_bitrate(&mut hello, codec_bit);
    cap_mode(&n, &mut hello);
    let (joined, reservation) = admit_mode(&n, &mut hello, codec).await?;

    crate::encode::validate_dimensions(codec, hello.mode.width, hello.mode.height)
        .context("client-requested mode")?;
    crate::encode::validate_refresh(hello.mode.refresh_hz).context("client-requested mode")?;

    let (mut compositor, mut gamescope_route) = negotiate_compositor(&n, &hello).await?;
    // A joiner streams the owner's display, so it runs on the owner's compositor and route.
    if let Some((d, _)) = joined.as_ref().filter(|(d, _)| d.compositor.is_some()) {
        compositor = d.compositor;
        gamescope_route = d.route.clone();
    }

    // Library launch is resolved after Welcome into `SessionContext.launch`. Do not write
    // `PUNKTFUNK_GAMESCOPE_APP`: process env races concurrent sessions and only gamescope's bare spawn reads it.

    // Env/cfg only; pads are created lazily by the input thread.
    let gamepad = resolve_gamepad(hello.gamepad, n.knobs.steam_force);

    // Capturer opens at this count (PipeWire pads silence; WASAPI AUTOCONVERTPCM up/downmixes).
    // Welcome echoes the value the audio thread will encode.
    let audio_channels = resolve_audio_channels(hello.audio_channels);
    tracing::info!(
        requested = hello.audio_channels,
        resolved = audio_channels,
        "audio channels"
    );

    let studio_asked = crate::session_plan::StudioAsk::of(&n.first.features);
    let (bit_depth, session_hdr, chroma) = negotiate_video_format(
        FormatFacts::of(
            &hello,
            studio_asked,
            compositor,
            gamescope_route.as_ref(),
            &n.knobs,
        ),
        codec,
        encoder.clone(),
        hello.mode,
    )
    .await?;

    // After depth + chroma: PyroWave Automatic is a ~bpp pin that scales with both, at the
    // client's quality when it sent one.
    let pyrowave_bpp = session_pyrowave_bpp(n.first.pyrowave_bpp_x100);
    let slices = crate::session_plan::client_slices(hello.video_caps, n.first.slices);
    let bitrate_kbps = resolve_bitrate_kbps_for(
        codec,
        hello.bitrate_kbps,
        &hello.mode,
        chroma,
        bit_depth,
        pyrowave_bpp,
        n.knobs.pyrowave_max_kbps,
    );
    tracing::info!(
        requested_kbps = hello.bitrate_kbps,
        resolved_kbps = bitrate_kbps,
        pyrowave_bpp,
        "encoder bitrate"
    );

    let shard_payload = shard_payload(&n, &hello).await;
    let chacha = wants_chacha(&hello);
    // AC-3 codes 7.1 as 5.1: Welcome, capture and encoder all take the plane's count.
    let (audio_plane, audio_channels) =
        negotiate_audio_plane(&n, &hello, audio_channels, bitrate_kbps).await?;
    let t = Terms {
        hello,
        codec_bit,
        codec,
        joined,
        compositor,
        gamescope_route,
        encoder,
        gamepad,
        audio_channels,
        bit_depth,
        hdr: session_hdr,
        chroma,
        studio_asked,
        pyrowave_bpp,
        slices,
        bitrate_kbps,
        shard_payload,
        chacha,
        audio_plane,
    };
    let server_hello = server_hello(&n, &t, welcome(&n, &t));
    let host_link = crate::telemetry::net_health::host_link(
        n.conn.local_ip(),
        n.conn.v2().map(|v2| &*v2.media_socket),
    );
    punktfunk_core::quic::v2::io::send(send, &server_hello).await?;
    n.bringup.mark("welcome");
    let prep = prepare_at_welcome(&n, &t, &server_hello.welcome);

    let (ty, body) = recv.read_frame().await?;
    punktfunk_core::quic::v2::msg::decode::<Ready>(ty, &body)
        .map_err(|e| anyhow!("Ready decode: {e:?}"))?;
    n.bringup.mark("start");
    // `wire_mtu::spawn_watch` is started by `serve_session` once the control-task channels
    // exist; it also drives mid-session shard renegotiation (needs the control writer).
    Ok(Negotiated {
        hello: t.hello,
        welcome: server_hello.welcome,
        client_label: n.first.client_label.clone(),
        preset: n.first.preset.clone().map(Into::into),
        abr_features: n.first.abr_features,
        client_link: n.first.link,
        probe_only: n.first.probe_only,
        pyrowave_bpp: t.pyrowave_bpp,
        slices: t.slices,
        host_link,
        compositor: t.compositor,
        gamescope_route: t.gamescope_route,
        prep,
        joined: t.joined,
        reservation: std::sync::Mutex::new(Some(reservation)),
        features: server_hello.features,
        resolved: n.resolved,
        encoder: t.encoder,
    })
}

/// The Welcome's shard payload. Before Welcome: a path a previous session proved jumbo is given
/// a bounded moment to re-prove itself on this connection (`negotiated_shard_payload` awaits it).
async fn shard_payload(n: &Negotiation<'_>, hello: &Hello) -> usize {
    let mut shard_payload =
        wire_mtu::negotiated_shard_payload(n.conn, hello.max_shard_payload, n.knobs.wire_mtu).await;
    // A browser's video rides this connection's datagrams, which are smaller than a UDP payload
    // (QUIC and HTTP/3 framing come out of the same budget). Ask, falling back to the 1200 every
    // QUIC path guarantees; a shard that does not fit is dropped at send, and FEC cannot cover all.
    // The budget is net of the kind varint, and the carrier seals, so a packet adds its header.
    if n.conn.is_web() {
        let budget = n.conn.max_datagram_size().unwrap_or(1200);
        let p = budget.saturating_sub(punktfunk_core::packet::V2_HEADER_LEN);
        shard_payload =
            shard_payload.min((p - p % 2).max(punktfunk_core::config::MIN_SHARD_PAYLOAD));
    }
    shard_payload
}

/// ChaCha20 when the client asked (`VIDEO_CAP_CHACHA20`, soft-AES armv7) and the operator
/// kill-switch allows. Both ends derive the key from the connection's exporter.
fn wants_chacha(hello: &Hello) -> bool {
    let client_wants_chacha = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_CHACHA20 != 0;
    let chacha = client_wants_chacha && pf_host_config::config().chacha20;
    tracing::info!(
        cipher = if chacha {
            "chacha20-poly1305"
        } else {
            "aes-128-gcm"
        },
        client_wants_chacha,
        "session cipher"
    );
    chacha
}

/// The Welcome for the settled terms: what the client opens its decoder, audio device and
/// input from.
fn welcome(n: &Negotiation<'_>, t: &Terms) -> Welcome {
    Welcome {
        mode: t.hello.mode,
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            // Static override pins it; else start mid-band and resize from the client's reports.
            fec_percent: n.knobs.fec_pct.unwrap_or(FEC_ADAPTIVE_START),
            max_data_per_block: 4096,
        },
        // Largest even payload whose sealed datagram fits an unfragmented UDP packet on a
        // 1500 MTU for this client's address family (1408 IPv4, 1388 IPv6 — v6 routers do
        // not fragment). Order: jumbo re-proof, `PUNKTFUNK_WIRE_MTU`, a prior session's
        // learned path budget, then this family default. See `wire_mtu.rs`.
        shard_payload: t.shard_payload as u16,
        frames: match n.source {
            Punktfunk1Source::Synthetic => n.frames,
            // Unbounded; the client streams until we close.
            Punktfunk1Source::SyntheticAbr(..)
            | Punktfunk1Source::Virtual
            | Punktfunk1Source::Software => 0,
        },
        // Auto for the synthetic source (no compositor).
        compositor: t
            .compositor
            .map(|c| c.as_pref())
            .unwrap_or(CompositorPref::Auto),
        gamepad: t.gamepad,
        bitrate_kbps: t.bitrate_kbps,
        bit_depth: t.bit_depth,
        // Mastering metadata (ST.2086 + CLL) rides the 0xCE datagram. The range is the one the
        // session plan encodes at.
        color: punktfunk_core::quic::ColorInfo::for_session(
            t.hdr,
            crate::session_plan::full_range_for(
                &t.encoder,
                t.studio_asked,
                t.codec,
                t.hdr,
                t.bit_depth,
            ),
        ),
        chroma_format: crate::encode::chroma_idc(t.chroma),
        audio_channels: t.audio_channels,
        // Negotiated codec; the client must not assume HEVC.
        codec: t.codec_bit,
        host_caps: welcome_host_caps(n, t),
        // `0` keeps the client's saved port: the standalone binary has no management API, and a
        // seat's answers on loopback only, so the box's stays the one its library comes from.
        mgmt_port: if pf_paths::seat::role().is_seat_host() {
            0
        } else {
            crate::mgmt::effective_port()
        },
        // Mask and remaining lifetime from admission. Full-control permanent is `GRANT_ALL, 0`.
        grants: n.grants,
        expires_in_secs: n.expires_in_secs,
        // Resolved plane. Client opens from these, never from what it asked.
        // `audio_frame_us` is `0` on Opus (fixed 5 ms).
        audio_codec: t.audio_plane.codec,
        audio_rate_hz: t.audio_plane.rate_hz,
        audio_bits: t.audio_plane.bits,
        audio_frame_us: t.audio_plane.frame_us,
        // The coupling the encoder runs.
        audio_layout: t.audio_plane.layout.wire(),
        host_caps2: welcome_host_caps2(n.source),
    }
}

/// The `ServerHello` around `welcome`. Settles the session's media suite, which keys the media,
/// and offers the native features this session can run.
fn server_hello(n: &Negotiation<'_>, t: &Terms, welcome: Welcome) -> ServerHello {
    // A browser's media goes unsealed, inside WebTransport's own encryption.
    let suite = (!n.conn.is_web()).then_some(if t.chacha {
        punktfunk_core::crypto::MediaSuite::ChaCha20Poly1305
    } else {
        punktfunk_core::crypto::MediaSuite::Aes128Gcm
    });
    let session = n.conn.v2_session();
    session.settle(suite);
    let mut offered = punktfunk_core::quic::v2::features::FeatureSet::default()
        .with(punktfunk_core::quic::v2::registry::FEATURE_STREAM_CONFIG)
        .with(punktfunk_core::quic::v2::registry::FEATURE_PAD_WRITES)
        .with(punktfunk_core::quic::v2::registry::FEATURE_PROFILES)
        .with(punktfunk_core::quic::v2::registry::FEATURE_CLEAR_PLACEMENT)
        .with(punktfunk_core::quic::v2::registry::FEATURE_FORMAT_EPOCH)
        .with(punktfunk_core::quic::v2::registry::FEATURE_AUDIO_EPOCH)
        .with(punktfunk_core::quic::v2::registry::FEATURE_SCROLL_AXIS_UNITS)
        .with(punktfunk_core::quic::v2::registry::FEATURE_STUDIO_RANGE)
        .with(punktfunk_core::quic::v2::registry::FEATURE_STUDIO_RANGE_HDR);
    // Further screens only where this session builds displays of its own on a compositor with
    // more than one output: not gamescope, not a joiner or a mirror.
    if matches!(n.source, Punktfunk1Source::Virtual)
        && t.compositor
            .is_some_and(|c| c != crate::vdisplay::Compositor::Gamescope)
        && t.joined.is_none()
        && !crate::session_plan::mirrored()
    {
        offered = offered.with(punktfunk_core::quic::v2::registry::FEATURE_SCREENS);
    }
    ServerHello {
        welcome,
        session_id: session.session_id,
        clock_origin_ns: session.clock.origin_ns(),
        suite,
        features: n.first.features.intersect(offered),
        profile: Some(n.resolved.id.clone()),
    }
}

/// Display prep once the Welcome is sent: its mode is final, and nothing in create→encoder
/// needs Start or the bound data plane. Windows only — Linux binds launch before create
/// (gamescope nests the command), which must not run if Start never arrives.
fn prepare_at_welcome(
    n: &Negotiation<'_>,
    t: &Terms,
    welcome: &Welcome,
) -> Option<super::stream::PrepHandle> {
    let compositor = match (n.source, t.compositor) {
        (Punktfunk1Source::Virtual, Some(comp)) if cfg!(target_os = "windows") => comp,
        _ => return None,
    };
    let prep = super::stream::DisplayPrep {
        compositor,
        mode: t.hello.mode,
        client_identity: n.conn.peer_fingerprint(),
        client_hdr: t.hello.display_hdr.map(crate::encode::hdr_meta_from_wire),
        // Read back off Welcome so the prepared display and session wiring cannot disagree.
        cursor_forward: welcome.host_caps & punktfunk_core::quic::HOST_CAP_CURSOR != 0,
        // The count SessionContext reads; a different one would change the wire mid-flow.
        slices: t.slices,
        bitrate_kbps: t.bitrate_kbps,
        // The build may re-resolve if the source delivers a different size.
        bitrate_auto: t.hello.bitrate_kbps == 0,
        bit_depth: t.bit_depth,
        hdr: t.hdr,
        studio_asked: t.studio_asked,
        // `bitrate_kbps` is the wire budget; the prep encoder opens at the derived video
        // rate, snapshotted at Welcome's initial FEC percent. The FEC watcher re-derives.
        enc_of: EncDerive {
            audio_kbps: audio_reserved_kbps(welcome),
            shard_payload: welcome.shard_payload,
            fec_percent: welcome.fec.fec_percent,
            head_only_hz: (t.codec == crate::encode::Codec::PyroWave)
                .then_some(welcome.mode.refresh_hz),
            // Further screens build after Welcome and take their shares then.
            share_permille: super::bitrate::SHARE_ALL,
        },
        chroma: t.chroma,
        codec: t.codec,
        pyrowave_bpp: t.pyrowave_bpp,
        pyrowave_max_kbps: n.knobs.pyrowave_max_kbps,
        shard_payload: welcome.shard_payload,
        encoder: t.encoder.clone(),
        // A joiner's view and fit, as `SessionContext::reframe_to` carries them.
        reframe_to: t.joined.as_ref().map(|(_, view)| {
            (
                punktfunk_core::video_fit::VideoFit::from_wire(t.hello.video_fit),
                *view,
            )
        }),
    };
    super::stream::spawn_display_prep(
        prep,
        n.quit.clone(),
        n.stop.clone(),
        n.bringup.clone(),
        n.prep_done.clone(),
    )
}

/// Waits until every stopped session has left the live set, its display lease dropped with it.
/// Capped at the 1.5 s grace the host once always slept: a stuck teardown costs no more than that.
async fn wait_released(stops: &[Arc<std::sync::atomic::AtomicBool>]) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1500);
    while !crate::vdisplay::admission::all_gone(stops) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// `encoder`'s codecs ∩ client advertised, honoring preference: the wire bit and the codec.
/// A client with no preference takes the Encoder row's pinned codec. A software host is
/// H.264-only — refuse rather than send a stream an HEVC-only client cannot decode.
fn pick_codec(
    hello: &Hello,
    encoder: &crate::encode::Resolved,
) -> Result<(u8, crate::encode::Codec)> {
    let host_codecs = encoder.wire_caps();
    let preferred_bit = match hello.preferred_codec {
        0 => encoder.pinned_codec(),
        p => p,
    };
    let codec_bit =
            punktfunk_core::quic::resolve_codec(hello.video_codecs, host_codecs, preferred_bit)
                .ok_or_else(|| {
                anyhow!(
                    "no shared video codec: client advertised 0x{:02x}, host can emit 0x{:02x} \
                     (a software-encode host produces H.264 — the client must advertise CODEC_H264)",
                    hello.video_codecs,
                    host_codecs
                )
            })?;
    let codec = crate::encode::codec_from_wire(codec_bit);
    // `preferred=` is the answer to "why not AV1?": the pick alone never says whether the
    // client asked for something else, and this is the line an operator greps.
    let preferred = codec_pref_label(hello.preferred_codec);
    tracing::info!(
        ?codec,
        preferred,
        client_codecs = format_args!("0x{:02x}", hello.video_codecs),
        host_codecs = format_args!("0x{host_codecs:02x}"),
        "video codec negotiated"
    );
    if let Some(miss) = punktfunk_core::quic::codec_preference_miss(
        hello.video_codecs,
        host_codecs,
        hello.preferred_codec,
    ) {
        tracing::info!(
            "{}",
            codec_miss_note(miss, preferred, punktfunk_core::hud::codec_label(codec_bit))
        );
    }
    Ok((codec_bit, codec))
}

/// The Hello's rate once the codec is known. The client reads the Welcome's codec through the
/// same rule, so both ends agree whether the session is Automatic, and every later read of
/// `hello.bitrate_kbps` sees the result.
fn settle_bitrate(hello: &mut Hello, codec_bit: u8) {
    let asked_kbps = hello.bitrate_kbps;
    hello.bitrate_kbps = punktfunk_core::quic::session_bitrate_kbps(
        asked_kbps,
        hello.preferred_codec,
        hello.video_codecs,
        codec_bit,
    );
    if hello.bitrate_kbps != asked_kbps {
        tracing::info!(
            asked_kbps,
            codec = punktfunk_core::hud::codec_label(codec_bit),
            "stored rate was for PyroWave — this session runs Automatic"
        );
    }
}

/// The operator's cap for this device, applied before mode-conflict and before Welcome: the
/// client is told the mode it actually gets, not a yes to 4K120 and then something else.
fn cap_mode(n: &Negotiation<'_>, hello: &mut Hello) {
    let fp = crate::vdisplay::policy::fp_hex(n.conn.peer_fingerprint());
    let want = (hello.mode.width, hello.mode.height, hello.mode.refresh_hz);
    let capped = crate::vdisplay::policy::prefs().cap_mode(fp.as_deref(), want);
    if capped != want {
        tracing::info!(
            requested = %format_args!("{}x{}@{}", want.0, want.1, want.2),
            granted = %format_args!("{}x{}@{}", capped.0, capped.1, capped.2),
            "per-device mode cap applied"
        );
        hello.mode.width = capped.0;
        hello.mode.height = capped.1;
        hello.mode.refresh_hz = capped.2;
    }
}

/// Mode-conflict before Welcome: stop this client's own earlier sessions, then join, steal or
/// refuse a live one. `Some` = joined: the owner's display and the size this client asked for.
/// Same-client reconnect never conflicts. The reservation is this connect's place in the live
/// set from here on: a later client conflicts with it, and a steal raises this session's `stop`.
async fn admit_mode(
    n: &Negotiation<'_>,
    hello: &mut Hello,
    codec: crate::encode::Codec,
) -> Result<(
    Option<(crate::vdisplay::admission::LiveDisplay, (u32, u32))>,
    crate::vdisplay::admission::Reservation,
)> {
    use crate::vdisplay::admission::{admit, preempt_resumed, preempt_same_identity, Admission};
    let peer_fp = n.conn.peer_fingerprint();

    // Own prior session (QUIC idle has not fired), and on `punktfunk/2` the exact session a
    // `resume` names. Stop them and wait for their release, so this reconnect reuses the
    // kept display. Runs before we register, so we never stop ourselves.
    let mut own_zombies = preempt_same_identity(peer_fp);
    if let Some(id) = n.first.resume {
        for s in preempt_resumed(id, peer_fp) {
            if !own_zombies.iter().any(|z| Arc::ptr_eq(z, &s)) {
                own_zombies.push(s);
            }
        }
    }
    if !own_zombies.is_empty() {
        tracing::info!(
            count = own_zombies.len(),
            "reconnect: preempting this client's own zombie session(s) so the kept display is reused"
        );
        for z in &own_zombies {
            z.store(true, Ordering::SeqCst);
        }
        wait_released(&own_zombies).await;
    }

    let mode = (hello.mode.width, hello.mode.height, hello.mode.refresh_hz);
    let (decision, reservation) = admit(peer_fp, mode, n.stop.clone());
    let joined = match decision {
        Admission::Separate => None,
        Admission::Join(m, display) => {
            tracing::info!(
                requested =
                    %format_args!("{}x{}@{}", hello.mode.width, hello.mode.height, hello.mode.refresh_hz),
                live = %format_args!("{}x{}@{}", m.0, m.1, m.2),
                "mode-conflict: JOIN — admitting at the live display's mode"
            );
            let view = (hello.mode.width, hello.mode.height);
            hello.mode.width = m.0;
            hello.mode.height = m.1;
            hello.mode.refresh_hz = m.2;
            // On gamescope the owner's game is the shared screen; this client's own title
            // would land in it.
            if display.compositor == Some(crate::vdisplay::Compositor::Gamescope)
                && hello.launch.take().is_some()
            {
                tracing::info!(
                    "mode-conflict: JOIN — this client's launch dropped; it shares the owner's game"
                );
            }
            Some((display, view))
        }
        Admission::Steal(victims) => {
            // A mode this session cannot encode fails below anyway: check it before the
            // victims lose their stream for nothing. A steal keeps the requested mode.
            crate::encode::validate_dimensions(codec, hello.mode.width, hello.mode.height)
                .context("client-requested mode")?;
            crate::encode::validate_refresh(hello.mode.refresh_hz)
                .context("client-requested mode")?;
            tracing::info!(
                victims = victims.len(),
                "mode-conflict: STEAL — preempting the live session(s)"
            );
            // The registry names the end first; the flag alone also reaches a victim
            // still in bring-up, and the compat plane's own flag.
            crate::session_status::stolen(&victims);
            for v in &victims {
                v.store(true, Ordering::SeqCst);
            }
            wait_released(&victims).await;
            None
        }
        Admission::Reject(reason) => {
            tracing::warn!("mode-conflict: REJECT — {reason}");
            // Out of the table before the refusal goes out, so a connect behind this one is
            // not refused by it. BUSY + reason bytes: the client reads `ApplicationClosed`,
            // not a bare drop, so the UI can name the live session.
            drop(reservation);
            n.conn.refuse(REJECT_BUSY_CODE, &reason).await;
            anyhow::bail!("{reason}");
        }
    };
    Ok((joined, reservation))
}

/// `Welcome::host_caps`: what this host does for this session. `0x80` is the last bit; the next
/// capability needs a second byte and an ABI bump.
fn welcome_host_caps(n: &Negotiation<'_>, t: &Terms) -> u8 {
    // Sequence-gated gamepad snapshots; capable clients send those, not per-transition events.
    punktfunk_core::quic::HOST_CAP_GAMEPAD_STATE
        // Clipboard only when operator policy and a platform backend both exist, and never to a
        // browser: its transfers ride quinn streams.
        | if pf_clipboard::cap_advertised() && n.conn.as_quic().is_some() {
            punktfunk_core::quic::HOST_CAP_CLIPBOARD
        } else {
            0
        }
        // Text injection only where the backend can type (SendInput UNICODE; wlroots keymap;
        // KWin keysyms). Clients without the bit keep VK-synthesis for IME.
        | if crate::inject::text_input_supported() {
            punktfunk_core::quic::HOST_CAP_TEXT_INPUT
        } else {
            0
        }
        // Client turns its local renderer on only when it sees this bit; serve_session
        // wires forwarding by reading the bit back.
        | if cursor_forward(
            t.hello.client_caps,
            t.compositor,
            t.codec,
            t.bit_depth,
            t.hdr,
            t.joined.is_some(),
            &t.encoder,
        ) {
            punktfunk_core::quic::HOST_CAP_CURSOR
        } else {
            0
        }
        // Pen batches → per-session uinput tablet. Without the bit, clients fold pen into
        // touch/pointer and `NativeClient::send_pen` refuses.
        | if crate::inject::pen_supported() && pen_reaches(t.compositor) {
            punktfunk_core::quic::HOST_CAP_PEN
        } else {
            0
        }
        // `0xD2` only when offered, budgeted, and on Opus: it is undefined on `0xD3` and `0xD4`.
        // Stated here, not left to the audio thread.
        | if t.audio_plane.codec == punktfunk_core::quic::AUDIO_CODEC_OPUS
            && audio_budget(
                redundancy_offered(t.hello.client_caps),
                t.bitrate_kbps,
                t.audio_channels,
                t.audio_plane.layout,
            )
            .redundancy
        {
            punktfunk_core::quic::HOST_CAP_AUDIO_RED
        } else {
            0
        }
        | if super::pad_audio::host_cap(t.hello.client_caps, n.knobs.pad_audio) {
            punktfunk_core::quic::HOST_CAP_PAD_AUDIO
        } else {
            0
        }
        // Set only when the gate resolved to PCM, not "host could".
        | if t.audio_plane.is_pcm() {
            punktfunk_core::quic::HOST_CAP_AUDIO_HIRES
        } else {
            0
        }
}

/// `Welcome::host_caps2`.
fn welcome_host_caps2(source: Punktfunk1Source) -> u8 {
    // Idle-keepalive re-encodes are marked `USER_FLAG_REPEAT` so client ABR treats an
    // unflagged AU as new content.
    punktfunk_core::quic::HOST_CAP2_REPEAT_MARK
        // The control loop feeds key edges into the input queue, so a client sends
        // them there and a lost release cannot hold a key.
        | punktfunk_core::quic::HOST_CAP2_INPUT_EDGES
        // Without the bit the client falls back to trackpad instead of sending contacts
        // the injector cannot land (wlroots) or cannot create a device for (Windows < 1809).
        | if crate::inject::touch_supported() {
            punktfunk_core::quic::HOST_CAP2_TOUCH
        } else {
            0
        }
        // Without the bit the client converts Scroll to MouseScroll before the wire.
        | if crate::inject::scroll_supported() {
            punktfunk_core::quic::HOST_CAP2_SCROLL
        } else {
            0
        }
        // Invites the client's `Start` extension block, which is where it names itself.
        | punktfunk_core::quic::HOST_CAP2_EXT
        // The virtual path binds its data plane two to three seconds before its
        // pipeline exists, and serves the client's bring-up ramp in that gap. The
        // rate-following source holds its first frame back for the same span
        // (`--bringup-ms`) so the rig measures the ramp the way a display session
        // runs it. The byte-pattern source has no such gap: video leaves at once.
        | match source {
            Punktfunk1Source::Virtual => punktfunk_core::quic::HOST_CAP2_RAMP,
            Punktfunk1Source::SyntheticAbr(shape) if shape.serve_ramp => {
                punktfunk_core::quic::HOST_CAP2_RAMP
            }
            _ => 0,
        }
}

/// Compositor for Welcome plus the gamescope route as a value; synthetic has neither. A seat's
/// own profile gets the seat's mode.
async fn negotiate_compositor(
    n: &Negotiation<'_>,
    hello: &Hello,
) -> Result<(
    Option<crate::vdisplay::Compositor>,
    Option<crate::vdisplay::GamescopeRoute>,
)> {
    let (client, profile) = (n.conn.peer_fingerprint(), &n.resolved);
    // Resolve now so Welcome reports the backend we will drive. Synthetic has no compositor.
    // Blocking probes → spawn_blocking.
    let compositor = match n.source {
        #[cfg(target_os = "linux")]
        Punktfunk1Source::Virtual if super::plays_seat_mode() => {
            Some(crate::seats::session_switch::connect_route())
        }
        Punktfunk1Source::Virtual => {
            let pref = hello.compositor;
            // Dedicated gamescope only if the launch id resolves to a command; an unknown id
            // must not spawn a blank "sleep infinity" gamescope. `launch_is_resolvable`, not
            // `resolve_launch`: a plugin command is loopback I/O and this is the async path.
            // A seat profile's bare connect opens its home, which is a launch too.
            let has_resolvable_launch = hello
                .launch
                .as_deref()
                .is_some_and(crate::library::launch_is_resolvable)
                || (hello.launch.is_none() && super::opens_big_picture(profile));
            let seat_profile =
                matches!(profile.os_account, crate::profiles::OsAccount::Seat { .. });
            let dedicated = crate::vdisplay::wants_dedicated_game_session(
                has_resolvable_launch,
                client,
                seat_profile,
            );
            Some(
                tokio::task::spawn_blocking(move || {
                    resolve_compositor(pref, dedicated, true, true)
                })
                .await
                .context("resolve compositor task")??,
            )
        }
        Punktfunk1Source::Synthetic
        | Punktfunk1Source::SyntheticAbr(..)
        | Punktfunk1Source::Software => None,
    };
    // Split the pair: compositor for Welcome/cursor; gamescope route as a value, not process env.
    let gamescope_route = compositor.as_ref().and_then(|(_, r)| r.clone());
    let compositor = compositor.map(|(c, _)| c);
    Ok((compositor, gamescope_route))
}

/// What the depth, HDR and chroma rule reads about the client, the source and the operator,
/// kept for the session so a codec change re-runs the handshake's rule for the new codec.
#[derive(Clone)]
pub(super) struct FormatFacts {
    pub(super) video_caps: u8,
    pub(super) compositor: Option<crate::vdisplay::Compositor>,
    pub(super) gamescope_route: Option<crate::vdisplay::GamescopeRoute>,
    /// The session's `PUNKTFUNK_10BIT_SDR_WIDEN` row.
    pub(super) sdr10_widen: bool,
    /// The colours the client asked to get at studio range.
    pub(super) studio_asked: crate::session_plan::StudioAsk,
}

impl FormatFacts {
    pub(super) fn of(
        hello: &Hello,
        studio_asked: crate::session_plan::StudioAsk,
        compositor: Option<crate::vdisplay::Compositor>,
        gamescope_route: Option<&crate::vdisplay::GamescopeRoute>,
        knobs: &SessionKnobs,
    ) -> FormatFacts {
        FormatFacts {
            video_caps: hello.video_caps,
            compositor,
            gamescope_route: gamescope_route.cloned(),
            sdr10_widen: knobs.sdr10_widen,
            studio_asked,
        }
    }

    /// Bit depth, HDR verdict, and chroma for `codec` at `mode`, each gated by host, client,
    /// codec, source, and `encoder`, the session's resolution. `mode` is the one the pipeline
    /// opens at: the Hello's at the handshake, the live one on a codec change. Blocks on its
    /// probes.
    pub(super) fn resolve(
        &self,
        codec: crate::encode::Codec,
        encoder: &crate::encode::Resolved,
        mode: punktfunk_core::Mode,
    ) -> (u8, bool, crate::encode::ChromaFormat) {
        let (compositor, gamescope_route) = (self.compositor, self.gamescope_route.as_ref());
        // 10-bit only when host, client, codec, capture, and GPU all allow it. Resolved before
        // Welcome so `color` matches the stream (a can't-10-bit GPU yields 8-bit SDR).
        let host_wants_10bit = pf_host_config::config().ten_bit;
        let client_supports_10bit = self.video_caps & punktfunk_core::quic::VIDEO_CAP_10BIT != 0;
        // `VIDEO_CAP_HDR` is BT.2020 PQ. `VIDEO_CAP_10BIT` alone is 10-bit SDR (Main10, display
        // untouched).
        let client_wants_hdr = self.video_caps & punktfunk_core::quic::VIDEO_CAP_HDR != 0;
        // Source-aware: Linux HDR depends on the compositor just resolved. Gamescope and direct
        // capture fold in `hdr_capture_failed(VirtualOutput)`; GameStream's rtsp.rs check has no
        // twin here: that latch is per-source and this gate already used this session's source.
        let capture_supports_hdr =
            crate::capture::capturer_supports_hdr_for(compositor, gamescope_route, encoder);
        // An 8-bit desktop's SDR-10 only widens: a colour pass per frame for no source detail, and
        // the operator opts in (`sdr10_widen`). Our gamescope composites 10-bit SDR itself, and
        // Windows 11 24H2 composes the virtual display in SDR wide colour (`sdr10_source`); neither
        // needs the opt-in. Either way the backend must write the 10 bits. Linux non-PyroWave 4:4:4
        // SDR is clamped to 8-bit below.
        let sdr10_widen = self.sdr10_widen;
        let sdr10_source = crate::capture::capturer_delivers_sdr10_for(compositor, gamescope_route);
        let sdr10_chain_ok = (sdr10_widen || sdr10_source)
            && codec_carries_sdr10(codec, sdr10_source)
            && encoder.carries_sdr10(codec);
        let depth_reachable = (client_wants_hdr && capture_supports_hdr) || sdr10_chain_ok;
        // Probe may open a tiny encoder: short-circuited behind the cheap gates.
        let gpu_can_10bit = if host_wants_10bit
            && client_supports_10bit
            && codec.supports_10bit()
            && depth_reachable
        {
            encoder.can_encode_10bit(codec)
        } else {
            false
        };
        let bit_depth: u8 = if gpu_can_10bit { 10 } else { 8 };
        // Colour label, capturer mandate, and vdisplay HDR all read this, never `bit_depth >= 10`
        // (10-bit without it is SDR: BT.709 VUI, display untouched).
        let session_hdr = gpu_can_10bit && client_wants_hdr && capture_supports_hdr;
        tracing::info!(
            bit_depth,
            session_hdr,
            host_wants_10bit,
            client_supports_10bit,
            client_wants_hdr,
            capture_supports_hdr,
            sdr10_widen,
            sdr10_source,
            sdr10_chain_ok,
            codec = ?codec,
            gpu_can_10bit,
            client_video_caps = self.video_caps,
            full_range = crate::session_plan::full_range_for(
                encoder,
                self.studio_asked,
                codec,
                session_hdr,
                bit_depth,
            ),
            "encode bit depth"
        );

        // 4:4:4 only when host, client, ingest chain, and GPU all allow it. Resolved before
        // Welcome so the client sizes its decoder from what we will actually emit.
        let host_wants_444 = pf_host_config::config().four_four_four;
        let client_supports_444 = self.video_caps & punktfunk_core::quic::VIDEO_CAP_444 != 0;
        // Ingest chain, not capturer: Windows 4:4:4 needs direct-NVENC RGB ingest (AMF cannot;
        // QSV/ffmpeg has no RGB 4:4:4 wiring). PyroWave does its own RGB→YCbCr; its gate is
        // `can_encode_444`. HDR does not cost chroma (HEVC Main 4:4:4 10).
        let ingest_chain_supports_444 = codec == crate::encode::Codec::PyroWave
            || crate::capture::capturer_supports_444(encoder.ingests_rgb_444());
        // Probe opens a tiny encoder, short-circuited. A negative latches until
        // restart — a GPU either supports HEVC 4:4:4 or it does not.
        let gpu_supports_444 = if matches!(
            codec,
            crate::encode::Codec::H265 | crate::encode::Codec::PyroWave
        ) && host_wants_444
            && client_supports_444
            && ingest_chain_supports_444
        {
            encoder.can_encode_444(codec)
        } else {
            false
        };
        // Client asked (VIDEO_CAP_444) but we still resolve 4:2:0: name the losing gate.
        if host_wants_444 && client_supports_444 && !gpu_supports_444 {
            let reason = if !matches!(
                codec,
                crate::encode::Codec::H265 | crate::encode::Codec::PyroWave
            ) {
                "the negotiated codec only carries 4:2:0 — 4:4:4 needs HEVC or PyroWave"
            } else if !ingest_chain_supports_444 {
                "this host's encoder backend can't ingest full chroma — 4:4:4 needs direct \
                 NVENC (NVIDIA) or the PyroWave codec"
            } else {
                "the GPU declined the 4:4:4 encode profile probe"
            };
            tracing::info!(reason, "4:4:4 requested but the session negotiates 4:2:0");
        }
        let chroma = if gpu_supports_444 {
            crate::encode::ChromaFormat::Yuv444
        } else {
            crate::encode::ChromaFormat::Yuv420
        };
        // PyroWave RDO packs the block index in 16 bits; ~8K 4:4:4 overflows. Downgrade to 4:2:0
        // before Welcome.
        let chroma = if codec == crate::encode::Codec::PyroWave
            && chroma.is_444()
            && !crate::encode::pyrowave_mode_fits_rdo(mode.width, mode.height, true)
        {
            tracing::warn!(
                mode = %format_args!("{}x{}", mode.width, mode.height),
                "PyroWave 4:4:4 at this mode exceeds the rate controller's block-index range — \
                 negotiating 4:2:0"
            );
            crate::encode::ChromaFormat::Yuv420
        } else {
            chroma
        };
        tracing::info!(
            chroma = ?chroma,
            host_wants_444,
            client_supports_444,
            ingest_chain_supports_444,
            "encode chroma"
        );

        #[cfg(target_os = "linux")]
        let bit_depth = linux_depth_under_444(bit_depth, chroma, session_hdr, codec);
        // Follows the depth clamp: an 8-bit stream is never labelled HDR.
        let session_hdr = session_hdr && bit_depth == 10;

        (bit_depth, session_hdr, chroma)
    }
}

/// [`FormatFacts::resolve`] off the runtime: its probes may open a tiny encoder.
async fn negotiate_video_format(
    facts: FormatFacts,
    codec: crate::encode::Codec,
    encoder: Arc<crate::encode::Resolved>,
    mode: punktfunk_core::Mode,
) -> Result<(u8, bool, crate::encode::ChromaFormat)> {
    tokio::task::spawn_blocking(move || facts.resolve(codec, &encoder, mode))
        .await
        .context("video format probe task")
}

/// Linux depth under 4:4:4. An 8-bit capture reaches NVENC 4:4:4 as planar 8-bit YUV444, so a
/// 10-bit SDR session encodes 8. HDR capture is packed 10-bit, which NVENC converts to 4:4:4
/// itself, and PyroWave's own CSC writes 10-bit 4:4:4: both keep 10.
#[cfg(target_os = "linux")]
fn linux_depth_under_444(
    bit_depth: u8,
    chroma: crate::encode::ChromaFormat,
    session_hdr: bool,
    codec: crate::encode::Codec,
) -> u8 {
    if !chroma.is_444() || bit_depth != 10 || session_hdr || codec == crate::encode::Codec::PyroWave
    {
        return bit_depth;
    }
    tracing::info!("4:4:4 SDR on the Linux path encodes 8-bit — resolving bit depth 8");
    8
}

/// Codecs that carry 10-bit SDR. PyroWave on Linux widens 8-bit RGB to 10-bit codes in the
/// pass that does the colour matrix; on Windows it takes only a real 10-bit source (SDR wide
/// colour), since its planes from an 8-bit desktop are 8-bit.
fn codec_carries_sdr10(codec: crate::encode::Codec, real_source: bool) -> bool {
    matches!(
        codec,
        crate::encode::Codec::H265 | crate::encode::Codec::Av1
    ) || (codec == crate::encode::Codec::PyroWave && (cfg!(target_os = "linux") || real_source))
}

/// Whether Hello carried a format at all. Decode maps an absent one to 48 kHz/16-bit, so
/// that pair (or a bare zero) is "nothing asked" — the rule `Hello::encode` applies.
fn audio_format_asked(rate_hz: u32, bits: u8) -> bool {
    use punktfunk_core::audio::{pcm, SAMPLE_RATE_HZ};
    (rate_hz != 0 && rate_hz != SAMPLE_RATE_HZ) || (bits != 0 && bits != pcm::BITS_16)
}

/// Audio plane for Welcome and the channel count it codes: AC-3 / E-AC-3 passthrough when the
/// client's sink asked and [`resolve_bitstream_plane`] holds; else Opus, or lossless PCM when
/// client, operator, and the capture-rate probe all allow it. Each in the coupling the client
/// asked for when this host knows it. Async for that blocking probe.
async fn negotiate_audio_plane(
    n: &Negotiation<'_>,
    hello: &Hello,
    audio_channels: u8,
    bitrate_kbps: u32,
) -> Result<(AudioPlane, u8)> {
    // `conn.max_datagram_size()` is quinn's current value; MTU discovery has not settled at
    // Welcome. Frame duration is a Welcome promise with no mid-session restatement, so the
    // conservative initial MTU is the safe direction: a frame that fits now keeps fitting as
    // MTU grows; a frame sized for a discovered MTU that then fails would not be sent.
    let max_datagram = n.conn.max_datagram_size();
    let hires_asked = hello.client_caps & punktfunk_core::quic::CLIENT_CAP_AUDIO_HIRES != 0;
    // A format without `CLIENT_CAP_AUDIO_HIRES` is contradictory (toggle vs resolved rate).
    // Ordinary "no capability" is silent; this one is logged because something asked.
    if !hires_asked && audio_format_asked(hello.audio_rate_hz, hello.audio_bits) {
        tracing::warn!(
            requested_rate_hz = hello.audio_rate_hz,
            requested_bits = hello.audio_bits,
            "client sent an audio format but not CLIENT_CAP_AUDIO_HIRES — ignoring it and \
             staying on Opus; the capability and the format must be set together"
        );
    }
    // Anything else is answered as legacy, which is also what an older client's absent byte
    // means.
    let layout = match punktfunk_core::audio::AudioLayout::from_wire(hello.audio_layout) {
        Some(layout) => layout,
        None => {
            tracing::info!(
                requested = hello.audio_layout,
                "audio layout unknown to this host — encoding legacy"
            );
            punktfunk_core::audio::AudioLayout::Legacy
        }
    };
    // Before the probe: a passthrough session never runs the lossless plane.
    if let Some((plane, channels)) = resolve_bitstream_plane(
        hello.audio_bitstreams,
        audio_channels,
        bitrate_kbps,
        max_datagram,
    ) {
        if hires_asked {
            tracing::info!("the client asked for hi-res audio and passthrough — passthrough wins");
        }
        return Ok((AudioPlane { layout, ..plane }, channels));
    }
    let hires_allowed = pf_host_config::config().audio_hires;
    // Honest capture rate, asked of the device (both backends resample without error).
    // Blocking on Windows, so spawn_blocking. Short-circuit on `hires_asked` first — the
    // operator gate is default-on, so that bit is the only thing sparing ordinary sessions
    // an endpoint enumeration. `Unknown` is correct when we did not ask.
    let capture_rate = if hires_asked && hires_allowed {
        tokio::task::spawn_blocking(crate::audio::probe_capture_rate)
            .await
            .context("audio capture-rate probe task")?
    } else {
        crate::audio::CaptureRate::Unknown
    };
    let plane = resolve_audio_plane(
        hires_asked,
        hires_allowed,
        hello.audio_rate_hz,
        hello.audio_bits,
        audio_channels,
        capture_rate,
        bitrate_kbps,
        max_datagram,
    );
    Ok((AudioPlane { layout, ..plane }, audio_channels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::audio::pcm;

    /// A rate stored for PyroWave reads as Automatic once the host lands on HEVC: the session
    /// resolves the host's default, and every later `== 0` test (the governor, a rebuild's
    /// re-resolve) sees Automatic, as the client does.
    #[test]
    fn a_pyrowave_rate_on_an_hevc_session_is_automatic() {
        use punktfunk_core::quic::{CODEC_HEVC, CODEC_PYROWAVE};
        let mut hello = Hello {
            mode: punktfunk_core::Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            compositor: punktfunk_core::config::CompositorPref::Auto,
            gamepad: punktfunk_core::config::GamepadPref::Auto,
            bitrate_kbps: 80_000,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: CODEC_HEVC | CODEC_PYROWAVE,
            preferred_codec: CODEC_PYROWAVE,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: punktfunk_core::audio::SAMPLE_RATE_HZ,
            audio_bits: pcm::BITS_16,
            audio_layout: 0,
            video_fit: 0,
            audio_bitstreams: 0,
        };
        let mut on_pyrowave = hello.clone();
        settle_bitrate(&mut on_pyrowave, CODEC_PYROWAVE);
        assert_eq!(on_pyrowave.bitrate_kbps, 80_000, "Fixed stays Fixed");
        settle_bitrate(&mut hello, CODEC_HEVC);
        assert_eq!(hello.bitrate_kbps, 0);
        let mode = hello.mode;
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::H265,
                hello.bitrate_kbps,
                &mode,
                crate::encode::ChromaFormat::Yuv420,
                8,
                session_pyrowave_bpp(0),
                None,
            ),
            20_000
        );
    }

    /// A lossless plane falls back to 48 kHz Opus in its own coupling, and its config anchors at
    /// the sequence the first Opus datagram takes.
    #[test]
    fn a_lossless_plane_falls_back_to_opus_in_its_coupling() {
        use punktfunk_core::audio::AudioLayout;
        use punktfunk_core::quic::{AUDIO_CODEC_OPUS, AUDIO_CODEC_PCM};
        let lossless = AudioPlane {
            codec: AUDIO_CODEC_PCM,
            rate_hz: 96_000,
            bits: pcm::BITS_24,
            frame_us: 2000,
            layout: AudioLayout::Standard,
        };
        let opus = lossless.opus_fallback();
        assert_eq!(
            (opus.codec, opus.rate_hz, opus.frame_us, opus.layout),
            (AUDIO_CODEC_OPUS, 48_000, 0, AudioLayout::Standard)
        );
        let cfg = opus.config(6, 1, 700, false);
        assert_eq!(
            (
                cfg.stream,
                cfg.epoch,
                cfg.codec,
                cfg.rate_hz,
                cfg.channels,
                cfg.first_seq
            ),
            (0, 1, AUDIO_CODEC_OPUS, 48_000, 6, 700)
        );
        assert_eq!(cfg.layout, AudioLayout::Standard.wire());
    }

    /// HDR keeps 10-bit at 4:4:4 on Linux; only an SDR H.26x session drops to 8.
    #[cfg(target_os = "linux")]
    #[test]
    fn hdr_keeps_its_depth_at_444_on_linux() {
        use crate::encode::ChromaFormat::{Yuv420, Yuv444};
        use crate::encode::Codec::{PyroWave, H265};
        assert_eq!(linux_depth_under_444(10, Yuv444, true, H265), 10);
        assert_eq!(linux_depth_under_444(10, Yuv444, false, H265), 8);
        assert_eq!(linux_depth_under_444(10, Yuv420, false, H265), 10);
        assert_eq!(linux_depth_under_444(10, Yuv444, false, PyroWave), 10);
        assert_eq!(linux_depth_under_444(8, Yuv444, false, H265), 8);
    }

    /// gamescope reads no tablet, so its sessions withhold `HOST_CAP_PEN` and the pen
    /// arrives as touch.
    #[test]
    fn pen_cap_skips_gamescope() {
        use crate::vdisplay::Compositor;
        assert!(!pen_reaches(Some(Compositor::Gamescope)));
        for c in [Compositor::Kwin, Compositor::Mutter, Compositor::Windows] {
            assert!(pen_reaches(Some(c)), "{c:?}");
        }
        assert!(pen_reaches(None));
    }

    /// The negotiation line names the preference, and a preference that lost says which
    /// side lacks it — the reporter's 0x0b client against a 0x0f host, and the reverse.
    #[test]
    fn a_lost_codec_preference_names_the_missing_side() {
        use punktfunk_core::quic::{
            codec_preference_miss, CodecMiss, CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE,
        };
        let client = CODEC_H264 | CODEC_HEVC | CODEC_PYROWAVE;
        let host = client | CODEC_AV1;
        assert_eq!(codec_pref_label(CODEC_AV1), "AV1");
        assert_eq!(codec_pref_label(0), "none");
        assert_eq!(codec_pref_label(0x40), "unknown");

        // No hardware AV1 decode: the host encodes it, the Hello never asked for it.
        let miss = codec_preference_miss(client, host, CODEC_AV1).expect("preference lost");
        assert_eq!(miss, CodecMiss::Client);
        let client_note = codec_miss_note(miss, "AV1", "HEVC");
        assert_eq!(
            client_note,
            "client prefers AV1 but does not advertise it (no decoder) — HEVC picked from \
             the shared set"
        );

        // The other direction: a software-encode host against an AV1-capable client.
        let miss = codec_preference_miss(host, client, CODEC_AV1).expect("preference lost");
        assert_eq!(miss, CodecMiss::Host);
        let host_note = codec_miss_note(miss, "AV1", "HEVC");
        assert!(host_note.contains("this host cannot encode it"));
        assert_ne!(client_note, host_note);

        // Neither side, and the two honoured cases that must stay silent.
        assert_eq!(
            codec_preference_miss(client, client, CODEC_AV1),
            Some(CodecMiss::Both)
        );
        assert_eq!(codec_preference_miss(client, host, CODEC_HEVC), None);
        assert_eq!(codec_preference_miss(client, host, 0), None);
        // A pre-negotiation client (no codec byte) decodes HEVC only.
        assert_eq!(
            codec_preference_miss(0, host, CODEC_AV1),
            Some(CodecMiss::Client)
        );
    }

    /// AV1 carries 10-bit SDR like HEVC; Linux PyroWave widens RGB→10-bit in its own CSC,
    /// so it is in there too. Windows PyroWave needs a real 10-bit source: its planes from an
    /// 8-bit desktop are 8-bit.
    #[test]
    fn sdr10_codecs_cover_hevc_av1_and_pyrowave_where_it_has_the_bits() {
        use crate::encode::Codec;
        for real in [false, true] {
            assert!(codec_carries_sdr10(Codec::Av1, real));
            assert!(codec_carries_sdr10(Codec::H265, real));
            assert!(!codec_carries_sdr10(Codec::H264, real));
        }
        assert_eq!(
            codec_carries_sdr10(Codec::PyroWave, false),
            cfg!(target_os = "linux")
        );
        assert!(codec_carries_sdr10(Codec::PyroWave, true));
    }

    /// 1472-byte discovery ceiling minus QUIC header + AEAD. Same number `pcm`'s ladder test uses.
    const DGRAM: usize = 1400;
    /// Above the 25 % allowance for every stereo rung (96/24 needs 18.4 Mbps of video).
    const FAT_LINK_KBPS: u32 = 40_000;
    /// Enough for every surround rung (176.4/24 7.1 wants ≥ 135 Mbps). Used only where the
    /// frame ladder is under test, so a row cannot fail on bandwidth first.
    const HUGE_LINK_KBPS: u32 = 200_000;
    /// Host-declared capture (Linux stream-sink). Condition-4 tests vary this; others hold it here.
    const HONEST_CAPTURE: crate::audio::CaptureRate = crate::audio::CaptureRate::Declared;

    /// Decode maps an absent format to 48 kHz/16-bit; that pair must not read as an ask.
    #[test]
    fn default_or_zero_format_is_not_an_ask() {
        assert!(!audio_format_asked(0, 0));
        assert!(!audio_format_asked(48_000, pcm::BITS_16));
        assert!(audio_format_asked(96_000, pcm::BITS_16));
        assert!(audio_format_asked(48_000, pcm::BITS_24));
    }

    /// Happy path; every decline test below is a difference from this.
    #[test]
    fn all_five_conditions_met_resolves_to_the_lossless_plane() {
        for (rate, bits) in [
            (48_000u32, pcm::BITS_16),
            (48_000, pcm::BITS_24),
            (96_000, pcm::BITS_16),
            (96_000, pcm::BITS_24),
        ] {
            let p = resolve_audio_plane(
                true,
                true,
                rate,
                bits,
                2,
                HONEST_CAPTURE,
                FAT_LINK_KBPS,
                Some(DGRAM),
            );
            assert!(p.is_pcm(), "{rate}/{bits} should have resolved to PCM");
            assert_eq!(p.rate_hz, rate);
            assert_eq!(p.bits, bits);
            // A frame that does not fit is never sent; this plane is never fragmented.
            assert!(
                pcm::frame_payload_bytes(rate, bits, 2, p.frame_us as u32) + pcm::PCM_HEADER_LEN
                    <= DGRAM,
                "{rate}/{bits} chose a {} µs frame that does not fit",
                p.frame_us
            );
        }
    }

    /// Client never asked. This is every ordinary session; it must be the quiet path to Opus.
    #[test]
    fn a_client_that_did_not_ask_gets_opus() {
        let p = resolve_audio_plane(
            false,
            true,
            96_000,
            pcm::BITS_24,
            2,
            HONEST_CAPTURE,
            FAT_LINK_KBPS,
            Some(DGRAM),
        );
        assert_eq!(p, AudioPlane::opus());
    }

    /// Operator gate off outranks a client that asks; the session stays on Opus.
    #[test]
    fn the_operator_gate_alone_can_decline() {
        let p = resolve_audio_plane(
            true,
            false,
            48_000,
            pcm::BITS_24,
            2,
            HONEST_CAPTURE,
            FAT_LINK_KBPS,
            Some(DGRAM),
        );
        assert_eq!(p, AudioPlane::opus());
    }

    /// Asserts the operator default, not that a session goes lossless. `CLIENT_CAP_AUDIO_HIRES`
    /// still gates every session. `config()` reads the process environment once.
    #[test]
    fn the_operator_default_is_on() {
        assert!(pf_host_config::config().audio_hires);
    }

    /// Surround is decided by the frame ladder, not a stereo-only rule. Runs on
    /// [`HUGE_LINK_KBPS`] so a declining row cannot fail on bandwidth first.
    #[test]
    fn surround_is_decided_by_the_frame_ladder() {
        // (channels, rate, bits, rung µs). `None` = decline.
        let matrix: [(u8, u32, u8, Option<u16>); 20] = [
            // 5.1. 44.1 kHz fits a longer rung than 48 kHz: a rung is a sample count.
            (6, 44_100, pcm::BITS_16, Some(2500)),
            (6, 44_100, pcm::BITS_24, Some(1500)),
            (6, 48_000, pcm::BITS_16, Some(2000)),
            (6, 48_000, pcm::BITS_24, Some(1500)), // ~667 packets/s
            (6, 88_200, pcm::BITS_16, Some(1000)),
            (6, 88_200, pcm::BITS_24, None),
            (6, 96_000, pcm::BITS_16, Some(1000)),
            (6, 96_000, pcm::BITS_24, None), // 1728 B/ms — over the datagram before the shortest rung
            (6, 176_400, pcm::BITS_16, None),
            (6, 176_400, pcm::BITS_24, None),
            // 7.1 is 4× stereo, so it runs out of ladder 4× sooner. Nothing above 48 kHz fits.
            (8, 44_100, pcm::BITS_16, Some(1500)),
            (8, 44_100, pcm::BITS_24, Some(1000)),
            (8, 48_000, pcm::BITS_16, Some(1500)),
            (8, 48_000, pcm::BITS_24, Some(1000)),
            (8, 88_200, pcm::BITS_16, None),
            (8, 88_200, pcm::BITS_24, None),
            (8, 96_000, pcm::BITS_16, None),
            (8, 96_000, pcm::BITS_24, None),
            (8, 176_400, pcm::BITS_16, None),
            (8, 176_400, pcm::BITS_24, None),
        ];
        for (ch, rate, bits, want_us) in matrix {
            let p = resolve_audio_plane(
                true,
                true,
                rate,
                bits,
                ch,
                HONEST_CAPTURE,
                HUGE_LINK_KBPS,
                Some(DGRAM),
            );
            match want_us {
                Some(us) => {
                    assert!(
                        p.is_pcm(),
                        "{ch}ch {rate}/{bits} should have resolved to PCM"
                    );
                    assert_eq!(p.frame_us, us, "{ch}ch {rate}/{bits} rung");
                    assert_eq!(p.rate_hz, rate);
                    assert_eq!(p.bits, bits);
                    // A frame over the path MTU is not sent; this plane is never fragmented.
                    assert!(
                        pcm::frame_payload_bytes(rate, bits, ch, us as u32) + pcm::PCM_HEADER_LEN
                            <= DGRAM,
                        "{ch}ch {rate}/{bits} chose a {us} µs frame that does not fit"
                    );
                }
                None => assert_eq!(
                    p,
                    AudioPlane::opus(),
                    "{ch}ch {rate}/{bits} must decline via the ladder, not be carried"
                ),
            }
        }
    }

    /// On an ordinary link surround declines on bandwidth before the ladder. 48/24 5.1 is
    /// 6 912 kbps and wants ≥ 27.6 Mbps of video.
    #[test]
    fn surround_still_needs_a_link_that_can_afford_it() {
        assert_eq!(
            resolve_audio_plane(
                true,
                true,
                48_000,
                pcm::BITS_24,
                6,
                HONEST_CAPTURE,
                20_000,
                Some(DGRAM)
            ),
            AudioPlane::opus(),
            "5.1 at 48/24 costs 6 912 kbps — more than a 20 Mbps session's 25 % allowance"
        );
        assert!(resolve_audio_plane(
            true,
            true,
            48_000,
            pcm::BITS_24,
            6,
            HONEST_CAPTURE,
            28_000,
            Some(DGRAM)
        )
        .is_pcm());
    }

    /// 44.1 kHz family. Refusing it now disagrees with `pcm::rate_is_supported` and with clients.
    #[test]
    fn the_44_1_khz_family_resolves_to_the_lossless_plane() {
        for (rate, bits, want_us) in [
            (44_100u32, pcm::BITS_16, 5000u16),
            (44_100, pcm::BITS_24, 5000),
            (88_200, pcm::BITS_16, 3000),
            (88_200, pcm::BITS_24, 2500),
            (176_400, pcm::BITS_16, 1500),
            (176_400, pcm::BITS_24, 1000),
        ] {
            let p = resolve_audio_plane(
                true,
                true,
                rate,
                bits,
                2,
                HONEST_CAPTURE,
                FAT_LINK_KBPS,
                Some(DGRAM),
            );
            assert!(p.is_pcm(), "{rate}/{bits} should have resolved to PCM");
            assert_eq!(p.rate_hz, rate, "the Welcome must state what was ASKED for");
            assert_eq!(p.bits, bits);
            assert_eq!(p.frame_us, want_us, "{rate}/{bits} rung");
            assert!(
                pcm::frame_payload_bytes(rate, bits, 2, p.frame_us as u32) + pcm::PCM_HEADER_LEN
                    <= DGRAM,
                "{rate}/{bits} chose a {} µs frame that does not fit",
                p.frame_us
            );
        }
        // Must not round 44 100 to 48 000 to make it fit. The Welcome must state what was asked.
        let p = resolve_audio_plane(
            true,
            true,
            44_100,
            pcm::BITS_24,
            2,
            HONEST_CAPTURE,
            FAT_LINK_KBPS,
            Some(DGRAM),
        );
        assert_eq!(p.rate_hz, 44_100);
    }

    /// Rates outside both families. 192 kHz is out of scope; 16 kHz is a voice rate this plane never offers.
    #[test]
    fn an_unsupported_format_gets_opus() {
        for rate in [192_000u32, 16_000] {
            let p = resolve_audio_plane(
                true,
                true,
                rate,
                pcm::BITS_24,
                2,
                HONEST_CAPTURE,
                FAT_LINK_KBPS,
                Some(DGRAM),
            );
            assert_eq!(p, AudioPlane::opus(), "{rate} Hz");
        }
        // The gate must read the set off core rather than restate it.
        for rate in [44_100u32, 48_000, 88_200, 96_000, 176_400] {
            assert!(pcm::rate_is_supported(rate), "{rate} Hz");
        }
        for rate in [0u32, 22_050, 32_000, 192_000] {
            assert!(!pcm::rate_is_supported(rate), "{rate} Hz");
        }
        for bits in [8u8, 20, 32] {
            let p = resolve_audio_plane(
                true,
                true,
                48_000,
                bits,
                2,
                HONEST_CAPTURE,
                FAT_LINK_KBPS,
                Some(DGRAM),
            );
            assert_eq!(p, AudioPlane::opus(), "{bits}-bit");
        }
    }

    /// Capture cannot deliver the rate: decline before Welcome. A silent pass would advertise
    /// 96 kHz and carry interpolated 48 kHz.
    #[test]
    fn a_capture_path_that_cannot_deliver_the_rate_gets_opus() {
        use crate::audio::CaptureRate;
        // Engine 48 kHz. `AUTOCONVERTPCM` would upsample 96; 96 declines, 48 is honoured.
        let engine_48 = CaptureRate::Engine(48_000);
        assert_eq!(
            resolve_audio_plane(
                true,
                true,
                96_000,
                pcm::BITS_24,
                2,
                engine_48,
                FAT_LINK_KBPS,
                Some(DGRAM)
            ),
            AudioPlane::opus(),
            "96 kHz on a 48 kHz engine must decline rather than pad"
        );
        assert!(
            resolve_audio_plane(
                true,
                true,
                48_000,
                pcm::BITS_24,
                2,
                engine_48,
                FAT_LINK_KBPS,
                Some(DGRAM)
            )
            .is_pcm(),
            "48 kHz on a 48 kHz engine is bit-exact and must be honoured"
        );
        // Engine above the request is fine (downsample). Decline only when the request is higher.
        assert!(resolve_audio_plane(
            true,
            true,
            48_000,
            pcm::BITS_16,
            2,
            CaptureRate::Engine(96_000),
            FAT_LINK_KBPS,
            Some(DGRAM)
        )
        .is_pcm());
        // Narrow endpoint cannot even do the base rate.
        assert_eq!(
            resolve_audio_plane(
                true,
                true,
                48_000,
                pcm::BITS_16,
                2,
                CaptureRate::Engine(24_000),
                FAT_LINK_KBPS,
                Some(DGRAM)
            ),
            AudioPlane::opus()
        );
        // Unknown (unread sink, or probe missed the endpoint): decline every rung.
        for (rate, bits) in [
            (48_000u32, pcm::BITS_16),
            (48_000, pcm::BITS_24),
            (96_000, pcm::BITS_16),
            (96_000, pcm::BITS_24),
        ] {
            assert_eq!(
                resolve_audio_plane(
                    true,
                    true,
                    rate,
                    bits,
                    2,
                    CaptureRate::Unknown,
                    FAT_LINK_KBPS,
                    Some(DGRAM)
                ),
                AudioPlane::opus(),
                "{rate}/{bits} with an unknowable capture rate"
            );
        }
    }

    /// [`CaptureRate`](crate::audio::CaptureRate) contract, pinned away from the gate.
    #[test]
    fn capture_rate_answers_only_what_it_can_prove() {
        use crate::audio::CaptureRate;
        // Host-declared format is honest at any rate the plane supports.
        assert!(CaptureRate::Declared.can_deliver(48_000));
        assert!(CaptureRate::Declared.can_deliver(96_000));
        // At-or-below the engine; equal is the normal pass, not a conservative edge.
        assert!(CaptureRate::Engine(96_000).can_deliver(96_000));
        assert!(CaptureRate::Engine(96_000).can_deliver(48_000));
        assert!(!CaptureRate::Engine(48_000).can_deliver(96_000));
        assert!(!CaptureRate::Engine(44_100).can_deliver(48_000));
        // Never yes without evidence.
        assert!(!CaptureRate::Unknown.can_deliver(48_000));
        assert!(!CaptureRate::Unknown.can_deliver(96_000));
    }

    /// Link cannot afford it. Audio is outside ABR, so the cost is off the top.
    #[test]
    fn a_link_that_cannot_afford_it_gets_opus() {
        // 5 Mbps affords nothing on the ladder.
        for (rate, bits) in [
            (48_000u32, pcm::BITS_16),
            (48_000, pcm::BITS_24),
            (96_000, pcm::BITS_16),
            (96_000, pcm::BITS_24),
        ] {
            let p = resolve_audio_plane(
                true,
                true,
                rate,
                bits,
                2,
                HONEST_CAPTURE,
                5_000,
                Some(DGRAM),
            );
            assert_eq!(p, AudioPlane::opus(), "{rate}/{bits} on a 5 Mbps session");
        }
        // 10 Mbps affords 48 kHz at either depth and neither 96 kHz rung.
        assert!(resolve_audio_plane(
            true,
            true,
            48_000,
            pcm::BITS_24,
            2,
            HONEST_CAPTURE,
            10_000,
            Some(DGRAM)
        )
        .is_pcm());
        assert_eq!(
            resolve_audio_plane(
                true,
                true,
                96_000,
                pcm::BITS_16,
                2,
                HONEST_CAPTURE,
                10_000,
                Some(DGRAM)
            ),
            AudioPlane::opus()
        );
        // Zero video bitrate can never afford it and must not divide by it.
        assert_eq!(
            resolve_audio_plane(
                true,
                true,
                48_000,
                pcm::BITS_16,
                2,
                HONEST_CAPTURE,
                0,
                Some(DGRAM)
            ),
            AudioPlane::opus()
        );
    }

    /// No datagrams, or a datagram too small for the shortest rung: fall back rather than
    /// emit a frame that would never be sent.
    #[test]
    fn a_datagram_that_cannot_carry_a_frame_gets_opus() {
        assert_eq!(
            resolve_audio_plane(
                true,
                true,
                48_000,
                pcm::BITS_16,
                2,
                HONEST_CAPTURE,
                FAT_LINK_KBPS,
                None
            ),
            AudioPlane::opus()
        );
        // 96/24 at 1000 µs is 576 B + 13 header; 200 B cannot carry it.
        assert_eq!(
            resolve_audio_plane(
                true,
                true,
                96_000,
                pcm::BITS_24,
                2,
                HONEST_CAPTURE,
                FAT_LINK_KBPS,
                Some(200)
            ),
            AudioPlane::opus()
        );
    }

    /// E-AC-3 when both are offered; AC-3 codes 7.1 as 5.1. Both run 48 kHz under their label.
    #[test]
    fn resolve_bitstream_plane_prefers_eac3_and_folds_ac3_71() {
        use punktfunk_core::audio::bitstream::{AUDIO_BITSTREAM_AC3, AUDIO_BITSTREAM_EAC3};
        use punktfunk_core::quic::{AUDIO_CODEC_AC3, AUDIO_CODEC_EAC3};
        let both = AUDIO_BITSTREAM_AC3 | AUDIO_BITSTREAM_EAC3;
        let (p, ch) = resolve_bitstream_plane(both, 8, FAT_LINK_KBPS, Some(DGRAM)).unwrap();
        assert_eq!((p.codec, ch, p.frame_us), (AUDIO_CODEC_EAC3, 8, 16_000));
        assert_eq!((p.rate_hz, p.bits), (48_000, pcm::BITS_16));
        assert!(p.is_bitstream() && !p.is_pcm());
        let ac3 = |ch| resolve_bitstream_plane(AUDIO_BITSTREAM_AC3, ch, FAT_LINK_KBPS, Some(DGRAM));
        let (p, ch) = ac3(8).unwrap();
        assert_eq!((p.codec, ch, p.frame_us), (AUDIO_CODEC_AC3, 6, 32_000));
        assert_eq!(ac3(3).unwrap().1, 2, "a count off the table is stereo");
    }

    /// No ask, no datagrams, or a link that cannot spare the fixed rate: no passthrough.
    #[test]
    fn resolve_bitstream_plane_declines_without_an_ask_datagrams_or_budget() {
        use punktfunk_core::audio::bitstream::AUDIO_BITSTREAM_EAC3;
        assert_eq!(
            resolve_bitstream_plane(0, 6, FAT_LINK_KBPS, Some(DGRAM)),
            None
        );
        assert_eq!(
            resolve_bitstream_plane(0x80, 6, FAT_LINK_KBPS, Some(DGRAM)),
            None,
            "a bit this host does not know"
        );
        let eac3 = |kbps, dgram| resolve_bitstream_plane(AUDIO_BITSTREAM_EAC3, 6, kbps, dgram);
        assert_eq!(eac3(FAT_LINK_KBPS, None), None);
        // E-AC-3 5.1 is 640 kbps, a quarter of 2 560.
        assert!(eac3(2_560, Some(DGRAM)).is_some());
        assert_eq!(eac3(2_559, Some(DGRAM)), None);
        assert_eq!(eac3(0, Some(DGRAM)), None);
    }

    /// Every plane the gate grants opens an omur stream: core's rate table is on omur's ladder.
    #[test]
    fn resolve_bitstream_plane_grants_only_what_omur_encodes() {
        use punktfunk_core::audio::bitstream::{self, AUDIO_BITSTREAM_AC3, AUDIO_BITSTREAM_EAC3};
        let mut rows = 0;
        for asked in [AUDIO_BITSTREAM_AC3, AUDIO_BITSTREAM_EAC3] {
            for channels in [2, 6, 8] {
                let (p, ch) =
                    resolve_bitstream_plane(asked, channels, HUGE_LINK_KBPS, Some(DGRAM)).unwrap();
                let config = super::super::audio::omur_config(p, ch).unwrap();
                assert_eq!(config.bitrate_kbps(), bitstream::bitrate_kbps(p.codec, ch));
                assert_eq!(
                    config.frame_samples(),
                    bitstream::frame_samples(p.codec, p.frame_us.into())
                );
                rows += usize::from(channels == ch);
            }
        }
        assert_eq!(rows, 5, "every non-zero row of the table");
    }

    /// Opus fallback is the pre-hi-res wire form; a drift would force every client to parse new fields.
    #[test]
    fn the_opus_fallback_is_the_legacy_wire_form() {
        let p = AudioPlane::opus();
        assert_eq!(p.codec, punktfunk_core::quic::AUDIO_CODEC_OPUS);
        assert_eq!(p.rate_hz, punktfunk_core::audio::SAMPLE_RATE_HZ);
        assert_eq!(p.bits, pcm::BITS_16);
        assert_eq!(p.frame_us, 0);
        assert!(!p.is_pcm());
    }
}
