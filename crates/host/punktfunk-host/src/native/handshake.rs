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
use crate::native::geometry::{Intent, Tenure};
use crate::vdisplay::policy::{DisplayTarget, Topology};
use punktfunk_core::abr::budget::FEC_ADAPTIVE_START;
use punktfunk_core::quic::v2::hello::{Role, RoleEcho, RoleReason, ScreenEcho, ScreenWish};

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
                    "audio_quality (PUNKTFUNK_AUDIO_QUALITY) is not one of low/standard/high — \
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
/// 176.4/24 7.1 (33.9 Mbps) needs ≥ 135.5. AC-3 5.1 (448 kbps) needs ≥ 1.8; E-AC-3 7.1
/// (1 024) ≥ 4.1.
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
/// resample without error. Channel count is decided only by the frame ladder: a frame that
/// fits one datagram ([`pcm::frame_us_for`]), else, for a client that joins parts
/// (`client_parts`), one that fits the send buffer in parts ([`pcm::parted_frame_us_for`]).
/// `max_datagram` is `None` when the peer has no datagrams, in which case there is no audio
/// plane.
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
    client_parts: bool,
) -> AudioPlane {
    use punktfunk_core::audio::pcm;
    // Not logged: every ordinary client skips this feature; the line would be noise.
    if !client_asked {
        return AudioPlane::opus();
    }
    if !operator_allows {
        tracing::info!(
            // The `audio_hires` row is off, from the console or PUNKTFUNK_AUDIO_HIRES=0.
            "hi-res audio requested but Lossless audio is off on this host \
             (PUNKTFUNK_AUDIO_HIRES) — the session uses Opus 48 kHz"
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
            "hi-res audio was requested but this host's capture path can't deliver that rate — \
             the session uses Opus 48 kHz. On Windows, set the rate in the device's sound \
             properties; on Linux, capture of the host's own output (Where audio plays, or \
             PUNKTFUNK_STREAM_SINK=0) offers only that sink's running rate"
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
    // Channel count is only a multiplier for the ladder; `None` is the decline. One datagram
    // first: a part lost costs its whole frame. Packet rate is not policed here — the
    // affordability gate above is in bits.
    let (rate, bits) = (requested_rate_hz, requested_bits);
    let single = pcm::frame_us_for(rate, bits, channels, max_datagram);
    let parted = || pcm::parted_frame_us_for(rate, bits, channels, max_datagram);
    let Some(frame_us) = single.or_else(|| parted().filter(|_| client_parts)) else {
        tracing::info!(
            requested_rate_hz,
            requested_bits,
            channels,
            max_datagram,
            client_parts,
            "no hi-res frame fits this connection — the session uses Opus 48 kHz. One datagram \
             cannot hold the shortest frame of this format, and either the client does not join \
             parts (an older one) or the parts together exceed the datagram send buffer \
             (176.4 kHz 7.1)"
        );
        return AudioPlane::opus();
    };
    tracing::info!(
        rate_hz = requested_rate_hz,
        bits = requested_bits,
        frame_us,
        // 1 is the `0xD3` datagram; more is a frame sent as `0xD5` parts.
        parts = match single {
            Some(_) => 1,
            None => pcm::frame_parts(rate, bits, channels, frame_us, max_datagram)
                .map_or(0, |(parts, _)| parts),
        },
        cost_kbps,
        video_kbps,
        max_datagram,
        // `Declared` vs `Engine(96000)` are different grounds for the same yes.
        ?capture_rate,
        "hi-res audio resolved — the session runs the lossless PCM plane"
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
/// Denied, the host still composites wherever the backend can blend. `worker` is a joiner's
/// or a mirror's session, whose picture a capture worker takes.
pub(super) fn cursor_forward(
    client_caps: u8,
    compositor: Option<crate::vdisplay::Compositor>,
    codec: crate::encode::Codec,
    bit_depth: u8,
    hdr: bool,
    worker: bool,
    encoder: &crate::encode::Resolved,
) -> bool {
    if client_caps & punktfunk_core::quic::CLIENT_CAP_CURSOR == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        // Same CUDA prediction `SessionPlan` makes; it picks the backend.
        let cuda_planned = !encoder.zero_copy_is_vaapi() && crate::zerocopy::enabled();
        let _ = worker;
        compositor.is_some_and(|c| c != crate::vdisplay::Compositor::Gamescope)
            && encoder.cursor_blend_capable(codec, cuda_planned, bit_depth == 10, hdr)
    }
    #[cfg(target_os = "windows")]
    {
        // Windows: the v5 IddCx hardware-cursor channel. Without it DWM paints the pointer
        // into the IDD frame and a second copy doubles it. The encoder is not consulted: the
        // driver blends the pointer on the capture-mouse flip; no Windows encode backend blends.
        // A capture worker's picture carries the pointer already.
        let _ = (compositor, codec, bit_depth, hdr, encoder);
        !worker && crate::windows::idd::hw_cursor_capable()
    }
    #[cfg(target_os = "macos")]
    {
        // Every session's stream is its own, a mirror's and a joiner's too, and paints the
        // pointer on the capture-mouse flip; no encoder blends. Its image must be readable now,
        // or the frames carry the pointer under the client's.
        let _ = (compositor, codec, bit_depth, hdr, worker, encoder);
        pf_capture::pointer_image_readable()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = (compositor, codec, bit_depth, hdr, worker, encoder);
        false
    }
}

/// Whether the session's compositor reads the uinput tablet pen batches land on. gamescope
/// reads none, so its clients send the pen as touch.
fn pen_reaches(compositor: Option<crate::vdisplay::Compositor>) -> bool {
    compositor != Some(crate::vdisplay::Compositor::Gamescope)
}

/// [`pen_reaches`], and the compositor has said it reads uinput devices: `negotiate_compositor`
/// asked it. A headless one reads none, and a device made for it would swallow its input.
fn uinput_reaches(compositor: Option<crate::vdisplay::Compositor>) -> bool {
    pen_reaches(compositor) && crate::inject::uinput_reach_known()
}

/// The touchpad features a session on `compositor` gets: the pad where the injector has one
/// and the pen reaches ([`pen_reaches`]: gamescope gets neither), taps only beside it.
fn touchpad_offer(
    compositor: Option<crate::vdisplay::Compositor>,
    supported: bool,
    taps: bool,
) -> punktfunk_core::quic::v2::features::FeatureSet {
    use punktfunk_core::quic::v2::registry::{FEATURE_TOUCHPAD, FEATURE_TOUCHPAD_TAPS};
    let mut set = punktfunk_core::quic::v2::features::FeatureSet::default();
    if supported && pen_reaches(compositor) {
        set = set.with(FEATURE_TOUCHPAD);
        if taps {
            set = set.with(FEATURE_TOUCHPAD_TAPS);
        }
    }
    set
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
    /// What the handshake settled, handed on whole to every part of the session.
    pub(super) terms: Arc<SessionTerms>,
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
    /// This host's end of the path, for every `StreamConfig`.
    pub(super) host_link: punktfunk_core::quic::HostLink,
    pub(super) prep: Option<super::stream::PrepHandle>,
    /// This connect's row in the admission table, until the planes register it. Dropped with
    /// the handshake when the session never gets that far.
    pub(super) reservation: std::sync::Mutex<Option<crate::vdisplay::admission::Reservation>>,
    /// Native feature bits in force ([`ServerHello::features`]).
    pub(super) features: punktfunk_core::quic::v2::features::FeatureSet,
    /// The profile this session plays as; `ServerHello` echoed its id.
    pub(super) resolved: crate::profiles::Resolved,
    /// The client asked for a game, and joining a shared screen dropped it.
    pub(super) launch_dropped: bool,
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

/// Everything the handshake settled, built once by [`negotiate`] and handed on as an `Arc`: the
/// Welcome states it, and the Welcome-time prep, the control task and the stream read it here.
pub(in crate::native) struct SessionTerms {
    /// The client's Hello at the mode it gets: capped, then admitted.
    pub(in crate::native) hello: Hello,
    codec_bit: u8,
    pub(in crate::native) codec: crate::encode::Codec,
    /// How this session holds its display.
    pub(in crate::native) tenure: Tenure,
    /// Admitted by `mode_conflict: join`: the owner's display, which this session shares.
    pub(in crate::native) joined: Option<crate::vdisplay::admission::LiveDisplay>,
    /// The mode the session gets, `hello.mode`, with the view it keeps and its framing.
    pub(in crate::native) intent: Intent,
    pub(in crate::native) compositor: Option<crate::vdisplay::Compositor>,
    /// Gamescope sub-mode as a value, not process env — a concurrent connect would overwrite env.
    pub(in crate::native) gamescope_route: Option<crate::vdisplay::GamescopeRoute>,
    /// The encode resolution the Welcome was negotiated against, kept for the session.
    pub(in crate::native) encoder: Arc<crate::encode::Resolved>,
    gamepad: GamepadPref,
    audio_channels: u8,
    /// The stream's depth, HDR verdict and chroma. Its `display_hdr` is the session's display
    /// lit in HDR; the Welcome's colour is the stream's, which is SDR while the content on that
    /// display is.
    pub(in crate::native) verdict: Verdict,
    /// The colours the client asked to get at studio range.
    pub(in crate::native) studio_asked: crate::session_plan::StudioAsk,
    /// PyroWave's bits per pixel for this session: the client's quality, else the host's.
    pub(in crate::native) pyrowave_bpp: f64,
    /// `PUNKTFUNK_PYROWAVE_MAX_MBPS`, kbps, over every PyroWave pin of the session.
    pub(in crate::native) pyrowave_max_kbps: Option<u32>,
    /// H.264/HEVC slices per frame ([`crate::session_plan::client_slices`]); 1 = single-slice.
    /// Amlogic decoders (Chromecast with Google TV) are reported to wedge on multi-slice.
    pub(in crate::native) slices: u32,
    pub(in crate::native) bitrate_kbps: u32,
    pub(in crate::native) shard_payload: usize,
    chacha: bool,
    audio_plane: AudioPlane,
    /// The Welcome grants the cursor channel. Read back off it, so the prepared display and the
    /// session wiring cannot disagree.
    pub(in crate::native) cursor_forward: bool,
    /// The audio reservation out of the wire budget, read off the Welcome.
    pub(in crate::native) audio_kbps: u32,
    /// What this session streams; `None` for a joiner, whose hello says `Shared`, or a synthetic
    /// source.
    screen: Option<ScreenEcho>,
    /// The screen this session wants ([`session_screen`]), which every open re-checks.
    pub(in crate::native) wanted: Option<crate::vdisplay::policy::DisplayTarget>,
    /// The topology its own screen opens under ([`session_screen`]); rebuilds reuse it.
    pub(in crate::native) topology: Option<crate::vdisplay::policy::Topology>,
    /// The role the Welcome states ([`role_echo`]).
    role: Option<RoleEcho>,
    /// The size the picture has on the client's glass, in millimetres. A cap or a game
    /// override fills the same glass, so it holds at every mode. `None` is unknown.
    pub(in crate::native) display_mm: Option<(u16, u16)>,
}

impl SessionTerms {
    /// The connector of the monitor this session mirrors, if it mirrors one.
    pub(in crate::native) fn mirrored(&self) -> Option<&str> {
        mirrored(self.screen.as_ref())
    }

    /// `Hello::bitrate_kbps == 0`. A mode switch re-resolves PyroWave's pin either way, and an
    /// Automatic rate is held inside the new bounds. Another codec's explicit rate stays.
    pub(in crate::native) fn bitrate_auto(&self) -> bool {
        self.hello.bitrate_kbps == 0
    }

    /// The client's panel, threaded into the EDID CTA HDR block before `create` so host apps
    /// tone-map to it. `None` from an older client or an SDR panel: the EDID and 0xCE take
    /// built-in defaults.
    pub(in crate::native) fn client_hdr(&self) -> Option<pf_frame::HdrMeta> {
        self.hello
            .display_hdr
            .map(crate::encode::hdr_meta_from_wire)
    }
}

/// `ClientHello` → `ServerHello` → `Ready`. Borrows the control streams; the caller keeps them
/// for mid-stream renegotiation. A joiner that can't stream the owner's HDR display, or the
/// format of a capture it shares, is refused before `ServerHello`.
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
    cap_mode(&n, &mut hello, codec);
    // The client's own intent, at the mode it gets before a join hands it the owner's.
    let asked = Intent::at_hello(hello.mode, n.first.view, &Tenure::of(None, false), codec);
    let asked_launch = hello.launch.is_some();
    let (joined, reservation) = admit_mode(&n, &mut hello, codec, asked.view()).await?;
    let launch_dropped = asked_launch && hello.launch.is_none();

    crate::encode::validate_dimensions(codec, hello.mode.width, hello.mode.height)
        .context("client-requested mode")?;
    crate::encode::validate_refresh(hello.mode.refresh_hz).context("client-requested mode")?;

    let (mut compositor, mut gamescope_route) = negotiate_compositor(&n, &hello).await?;
    // A joiner streams the owner's display, so it runs on the owner's compositor and route.
    if let Some((d, _)) = joined.as_ref().filter(|(d, _)| d.compositor.is_some()) {
        compositor = d.compositor;
        gamescope_route = d.route.clone();
    }
    let SessionScreen {
        wanted,
        echo: screen,
        topology,
        role,
    } = session_screen(&n, joined.is_some(), compositor).await?;
    let fit = punktfunk_core::video_fit::VideoFit::from_wire(hello.video_fit);
    let tenure = Tenure::of(
        joined.as_ref().map(|(_, view)| (fit, *view)),
        matches!(screen, Some(ScreenEcho::Monitor(_))),
    );
    let joined = joined.map(|(display, _)| display);
    hello.mode = deliverable_rate(
        hello.mode,
        runs_exact_rate(compositor, tenure.shares_capture()),
    );
    let intent = Intent::at_hello(hello.mode, asked.view(), &tenure, codec);

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

    let facts = FormatFacts::of(
        &hello,
        crate::session_plan::StudioAsk::of(&n.first.features),
        compositor,
        gamescope_route.as_ref(),
        &n.knobs,
        joined.as_ref(),
        follows_source(&n.first.features, &tenure),
        mirrored(screen.as_ref()),
    );
    let studio_asked = facts.studio_asked;
    // The range a joiner must stream: a shared capture's own, on a display lit either way.
    let owner_stream = joined.as_ref().map(|d| d.stream_hdr());
    let verdict = negotiate_video_format(facts.clone(), codec, encoder.clone(), hello.mode).await?;
    let Verdict {
        bit_depth,
        hdr: session_hdr,
        chroma,
        ..
    } = verdict;
    if owner_stream
        .is_some_and(|o| crate::vdisplay::admission::joined_hdr(o, session_hdr).is_none())
    {
        // Out of the table first, as `admit_mode`'s refusal.
        drop(reservation);
        let said = super::Said::new(
            "close_join_needs_hdr",
            crate::vdisplay::admission::JOIN_NEEDS_HDR,
        );
        n.conn.refuse_said(REJECT_BUSY_CODE, &said).await;
        anyhow::bail!("join refused: the shared display is HDR and this session can't be");
    }
    let resolved = (bit_depth, session_hdr, chroma);
    if let Some(Err(gap)) = facts.joined_format(codec, &encoder, resolved) {
        drop(reservation);
        let (id, text) = gap.said();
        n.conn
            .refuse_said(REJECT_BUSY_CODE, &super::Said::new(id, text))
            .await;
        anyhow::bail!("join refused: this session can't take the shared capture's {gap:?}");
    }

    // After depth + chroma: PyroWave Automatic is a ~bpp pin that scales with both, at the
    // client's quality when it sent one.
    let pyrowave_bpp = session_pyrowave_bpp(n.first.pyrowave_bpp_x100);
    let slices = crate::session_plan::client_slices(hello.video_caps, n.first.slices);
    // Priced at the picture the encoder means to take, as every build of the session prices it.
    let bitrate_kbps = resolve_bitrate_kbps_for(
        codec,
        hello.bitrate_kbps,
        &intent.picture(),
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
    let display_mm = n.first.display_mm;
    let mut terms = SessionTerms {
        hello,
        codec_bit,
        codec,
        tenure,
        joined,
        intent,
        compositor,
        gamescope_route,
        encoder,
        gamepad,
        audio_channels,
        verdict,
        studio_asked,
        pyrowave_bpp,
        pyrowave_max_kbps: n.knobs.pyrowave_max_kbps,
        slices,
        bitrate_kbps,
        shard_payload,
        chacha,
        audio_plane,
        // Both are read off the Welcome below.
        cursor_forward: false,
        audio_kbps: 0,
        screen,
        wanted,
        topology,
        role,
        display_mm,
    };
    let welcome = welcome(&n, &terms);
    terms.cursor_forward = welcome.host_caps & punktfunk_core::quic::HOST_CAP_CURSOR != 0;
    terms.audio_kbps = audio_reserved_kbps(&welcome);
    let terms = Arc::new(terms);
    let server_hello = server_hello(&n, &terms, welcome);
    let host_link = crate::telemetry::net_health::host_link(
        n.conn.local_ip(),
        n.conn.v2().map(|v2| &*v2.media_socket),
    );
    punktfunk_core::quic::v2::io::send(send, &server_hello).await?;
    n.bringup.mark("welcome");
    let prep = prepare_at_welcome(&n, &terms, &server_hello.welcome);

    let (ty, body) = recv.read_frame().await?;
    punktfunk_core::quic::v2::msg::decode::<Ready>(ty, &body)
        .map_err(|e| anyhow!("Ready decode: {e:?}"))?;
    n.bringup.mark("start");
    // `wire_mtu::spawn_watch` is started by `wire_session` once the control-task channels
    // exist; it also drives mid-session shard renegotiation (needs the control writer).
    Ok(Negotiated {
        terms,
        welcome: server_hello.welcome,
        client_label: n.first.client_label.clone(),
        preset: n.first.preset.clone().map(Into::into),
        abr_features: n.first.abr_features,
        client_link: n.first.link,
        probe_only: n.first.probe_only,
        host_link,
        prep,
        reservation: std::sync::Mutex::new(Some(reservation)),
        features: server_hello.features,
        resolved: n.resolved,
        launch_dropped,
    })
}

/// What [`session_screen`] settles.
#[derive(Default)]
struct SessionScreen {
    /// The screen wanted, which every open re-checks.
    wanted: Option<DisplayTarget>,
    /// The screen the Welcome states.
    echo: Option<ScreenEcho>,
    /// The topology an own screen opens under.
    topology: Option<Topology>,
    /// The role the Welcome states ([`role_echo`]).
    role: Option<RoleEcho>,
}

/// The screen this session wants, the one the Welcome states, the topology of its own screen
/// ([`crate::vdisplay::session_topology`]) and its role, decided once and logged. A joiner
/// streams the owner's display and states neither; a synthetic source has no screen, so a wish
/// there reads as no monitors. Blocks on a compositor read.
async fn session_screen(
    n: &Negotiation<'_>,
    joined: bool,
    compositor: Option<crate::vdisplay::Compositor>,
) -> Result<SessionScreen> {
    let may_choose = n.grants & punktfunk_core::quic::GRANT_DISPLAY != 0;
    let wish = n.first.screen_wish.clone();
    let settled = if matches!(n.source, Punktfunk1Source::Virtual) {
        let client = n.conn.peer_fingerprint();
        let wish = wish.clone();
        tokio::task::spawn_blocking(move || {
            // Only a wish pays for the monitor read: every connect would wait on it otherwise.
            let desk = !joined && crate::vdisplay::desk(compositor, wish.is_some());
            settle_screen(client, may_choose, wish, joined, compositor, desk)
        })
        .await
        .context("resolve screen task")?
    } else {
        let role = role_echo(wish.is_some(), may_choose, false, None, false, false);
        SessionScreen {
            role,
            ..SessionScreen::default()
        }
    };
    let role = settled.role;
    tracing::info!(
        wish = wish.as_ref().map_or_else(|| "none".into(), ToString::to_string),
        role = ?role.and_then(|r| r.role),
        reason = ?role.map(|r| r.reason),
        "session screen role"
    );
    Ok(settled)
}

/// [`session_screen`] for a virtual display. The Hello's `wish` comes first while it counts
/// ([`asked`]), then the device's pick with the grant (`may_choose`), then the operator's. A
/// monitor that cannot be mirrored now is stated as the session's own screen.
fn settle_screen(
    client: Option<[u8; 32]>,
    may_choose: bool,
    wish: Option<ScreenWish>,
    joined: bool,
    compositor: Option<crate::vdisplay::Compositor>,
    desk: bool,
) -> SessionScreen {
    let fp = crate::vdisplay::policy::fp_hex(client);
    let (row, pinned) = crate::vdisplay::screen_target(fp.as_deref(), may_choose);
    let primary = compositor.is_some_and(crate::vdisplay::Compositor::has_primary);
    let asked = asked(wish.as_ref(), may_choose, desk, pinned, primary);
    let wanted = asked.as_ref().map_or(row, |(screen, _)| screen.clone());
    if joined {
        return SessionScreen {
            wanted: Some(wanted),
            ..SessionScreen::default()
        };
    }
    let pick = crate::vdisplay::resolve_screen(client, Some(wanted.clone()));
    let mirrored = crate::vdisplay::mirrors(&pick.target);
    let echo = match pick.target.into_monitor() {
        Some(c) if mirrored => ScreenEcho::Monitor(c),
        _ => ScreenEcho::Own,
    };
    // A wished monitor that fell back.
    let unavailable = asked.is_some() && pick.reason.is_some();
    let (mut topology, mut held, mut got) = (None, false, None);
    if let (ScreenEcho::Own, Some(c)) = (&echo, compositor) {
        let want = asked.and_then(|(_, t)| t);
        let want = want.or_else(|| crate::vdisplay::wanted_topology(fp.as_deref(), may_choose));
        (topology, held) = crate::vdisplay::session_topology(client, want, c);
        let applied = topology.unwrap_or_else(|| crate::vdisplay::effective_topology(client));
        // Monitors this session wants on stay off under an earlier `only` screen.
        held |= applied != Topology::Exclusive
            && crate::vdisplay::monitors_off_by()
                .iter()
                .flatten()
                .any(|d| Some(d) != fp.as_ref());
        got = Some(crate::vdisplay::role_of(applied, primary));
    }
    let allowed = may_choose && !pinned;
    SessionScreen {
        wanted: Some(wanted),
        role: role_echo(wish.is_some(), allowed, desk, got, unavailable, held),
        echo: Some(echo),
        topology,
    }
}

/// What the Hello's `wish` asks while it counts: the screen, and the topology its own screen
/// opens under (`None` for a monitor). It counts with the grant (`may_choose`), on the `desk`,
/// and without the env pin (`pinned`). `main` is `second` where the desktop has no main screen
/// (`primary`). Nothing is stored.
fn asked(
    wish: Option<&ScreenWish>,
    may_choose: bool,
    desk: bool,
    pinned: bool,
    primary: bool,
) -> Option<(DisplayTarget, Option<Topology>)> {
    let own = |t| Some((DisplayTarget::Own, Some(t)));
    match wish.filter(|_| may_choose && desk && !pinned)? {
        ScreenWish::Only => own(Topology::Exclusive),
        ScreenWish::Main if primary => own(Topology::Primary),
        ScreenWish::Main | ScreenWish::Second => own(Topology::Extend),
        ScreenWish::Monitor(c) => Some((DisplayTarget::Monitor(c.clone()), None)),
    }
}

/// Tag 30: the role `got` (`None` for a copy of a monitor) and why it is not what a `wish`, or
/// with none nothing, asked. Off the `desk` there is no role, and only a wish is answered.
/// `allowed`: the device may choose and no env pin decides. `unavailable`: a wished monitor
/// fell back. `held`: an earlier session decided the monitors. Precedence 1, 4, 3, 2.
fn role_echo(
    wish: bool,
    allowed: bool,
    desk: bool,
    got: Option<Role>,
    unavailable: bool,
    held: bool,
) -> Option<RoleEcho> {
    let reason = match () {
        _ if wish && !allowed => RoleReason::NotAllowed,
        _ if wish && !desk => RoleReason::NoMonitors,
        _ if !desk => return None,
        _ if unavailable => RoleReason::Unavailable,
        _ if held => RoleReason::MonitorsHeld,
        _ => RoleReason::AsAsked,
    };
    let role = got.filter(|_| desk);
    Some(RoleEcho { role, reason })
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
fn welcome(n: &Negotiation<'_>, t: &SessionTerms) -> Welcome {
    Welcome {
        mode: t.hello.mode,
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            // Static override pins it; else start mid-band and resize from the client's reports.
            fec_percent: n.knobs.fec_pct.unwrap_or(FEC_ADAPTIVE_START),
            max_data_per_block: 4096,
        },
        // Order: jumbo re-proof, `PUNKTFUNK_WIRE_MTU`, a prior session's learned path
        // budget, then the address family's default (`config::mtu1500_shard_payload_for`).
        // A browser's datagram size clamps it after. See `wire_mtu.rs`.
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
        bit_depth: t.verdict.bit_depth,
        // Mastering metadata (ST.2086 + CLL) rides the 0xCE datagram. The range is the one the
        // session plan encodes at.
        color: punktfunk_core::quic::ColorInfo::for_session(
            t.verdict.hdr,
            crate::session_plan::full_range_for(
                &t.encoder,
                t.studio_asked,
                t.codec,
                t.verdict.hdr,
                t.verdict.bit_depth,
            ),
        ),
        chroma_format: crate::encode::chroma_idc(t.verdict.chroma),
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

/// Whether a session on gamescope can move to a desktop mid-stream. A seat's own host follows
/// its seat's switches. The box's host moves to the live session on a Gaming/Desktop switch or a
/// capture loss, unless `PUNKTFUNK_COMPOSITOR` pins the backend.
fn can_leave_gamescope() -> bool {
    #[cfg(target_os = "linux")]
    if crate::native::plays_seat_mode() {
        return true;
    }
    pf_host_config::config().compositor.is_none()
}

/// The `ServerHello` around `welcome`. Settles the session's media suite, which keys the media,
/// offers the native features this session can run, states its screen and role, and tells a
/// joiner its screen is shared.
fn server_hello(n: &Negotiation<'_>, t: &SessionTerms, welcome: Welcome) -> ServerHello {
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
        .with(punktfunk_core::quic::v2::registry::FEATURE_STUDIO_RANGE_HDR)
        .union(touchpad_offer(
            t.compositor,
            crate::inject::touchpad_supported() && uinput_reaches(t.compositor),
            crate::inject::touchpad_taps(),
        ));
    // Further screens only where this session builds displays of its own. gamescope has one
    // output: there the screen set holds them back until the session moves to a desktop, so a
    // session that can never leave gamescope is not offered them.
    if matches!(n.source, Punktfunk1Source::Virtual)
        && t.compositor
            .is_some_and(|c| c != crate::vdisplay::Compositor::Gamescope || can_leave_gamescope())
        && t.tenure.offers_screens()
    {
        offered = offered.with(punktfunk_core::quic::v2::registry::FEATURE_SCREENS);
    }
    // The bit the audio plane resolved on ([`pcm_parts`]): the sender reads it back.
    if !n.conn.is_web() {
        offered = offered.with(punktfunk_core::quic::v2::registry::FEATURE_PCM_PARTS);
    }
    ServerHello {
        // The picture the Welcome's rate was priced at.
        picture: t.intent.picture().wire(welcome.mode),
        welcome,
        session_id: session.session_id,
        clock_origin_ns: session.clock.origin_ns(),
        suite,
        features: n.first.features.intersect(offered),
        profile: Some(n.resolved.id.clone()),
        screen: if t.tenure.join_live() {
            Some(ScreenEcho::Shared)
        } else {
            t.screen.clone()
        },
        role: t.role,
    }
}

/// Display prep once the Welcome is sent: its mode is final, and nothing in create→encoder
/// needs Start or the bound data plane. Windows only — Linux binds launch before create
/// (gamescope nests the command), which must not run if Start never arrives.
fn prepare_at_welcome(
    n: &Negotiation<'_>,
    t: &Arc<SessionTerms>,
    welcome: &Welcome,
) -> Option<super::stream::PrepHandle> {
    if !(matches!(n.source, Punktfunk1Source::Virtual)
        && t.compositor.is_some()
        && cfg!(target_os = "windows"))
    {
        return None;
    }
    let prep = super::stream::DisplayPrep {
        terms: t.clone(),
        client_identity: n.conn.peer_fingerprint(),
        // `bitrate_kbps` is the wire budget; the prep encoder opens at the derived video
        // rate, snapshotted at Welcome's initial FEC percent. The FEC watcher re-derives.
        // Further screens build after Welcome and take their shares then.
        enc_of: EncDerive::new(
            t,
            t.hello.mode.refresh_hz,
            welcome.fec.fec_percent,
            super::bitrate::SHARE_ALL,
        ),
    };
    super::stream::spawn_display_prep(
        prep,
        n.quit.clone(),
        n.stop.clone(),
        n.bringup.clone(),
        n.prep_done.clone(),
    )
}

/// The longest a connect waits for the sessions it stopped: a stuck teardown costs it no more.
const RELEASE_CAP: std::time::Duration = std::time::Duration::from_millis(1500);

/// Waits until every stopped session has left the live set, its display lease dropped with it,
/// or [`RELEASE_CAP`] has passed.
async fn wait_released(stops: &[Arc<std::sync::atomic::AtomicBool>]) {
    wait_released_within(stops, RELEASE_CAP).await
}

/// [`wait_released`] under `cap`.
async fn wait_released_within(
    stops: &[Arc<std::sync::atomic::AtomicBool>],
    cap: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + cap;
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

/// Whether a session's own display on `compositor` runs a rate that is not whole hertz. A
/// shared display runs its owner's rate, which the stream then reports. No display at all
/// paces as asked.
pub(crate) fn runs_exact_rate(
    compositor: Option<crate::vdisplay::Compositor>,
    shared: bool,
) -> bool {
    !shared && compositor.is_none_or(|c| c.runs_fractional_rates())
}

/// `mode` at a rate this session delivers: its fraction where the display runs one
/// ([`runs_exact_rate`]), whole hertz elsewhere. A fraction left over from before a cap or a
/// join moved the whole rate names another rate and goes either way.
pub(crate) fn deliverable_rate(mode: punktfunk_core::Mode, exact: bool) -> punktfunk_core::Mode {
    let stated = mode.stating(mode.refresh_mhz);
    if exact {
        stated
    } else {
        stated.at_millihz(stated.refresh_hz.saturating_mul(1000))
    }
}

/// The operator's cap for this device, then the launched game's overrides in the session
/// profile's view, applied before mode-conflict and before Welcome: the client is told the mode
/// it actually gets, not a yes to 4K120 and then something else.
fn cap_mode(n: &Negotiation<'_>, hello: &mut Hello, codec: crate::encode::Codec) {
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
    let Some(game) = hello.launch.clone() else {
        return;
    };
    let view = crate::library::View::session(&n.resolved);
    let Some(o) = crate::library::overrides_for(&view, &game) else {
        return;
    };
    let asked = (hello.mode, hello.video_caps, hello.gamepad);
    let held = game_override_terms(asked, &o, codec.max_dimension());
    if held != asked {
        let m = |m: punktfunk_core::Mode| format!("{}x{}@{}", m.width, m.height, m.refresh_hz);
        tracing::info!(
            game = %game,
            requested = %m(asked.0),
            granted = %m(held.0),
            hdr_off = held.1 != asked.1,
            gamepad = held.2.as_str(),
            "game override applied"
        );
        (hello.mode, hello.video_caps, hello.gamepad) = held;
    }
}

/// The half of a game override the host holds a client to: a lower mode, the width keeping
/// the request's aspect; HDR off; the pad it names. It never raises: no panel is known here.
fn game_override_terms(
    (mode, caps, pad): (punktfunk_core::Mode, u8, GamepadPref),
    o: &punktfunk_core::game_overrides::GameOverrides,
    codec_max: u32,
) -> (punktfunk_core::Mode, u8, GamepadPref) {
    use punktfunk_core::game_overrides::{fit, GameOverrides};
    let down = GameOverrides {
        lines: o.lines.filter(|&l| l < mode.height),
        hz: o.hz.filter(|&z| z < mode.refresh_hz),
        ..GameOverrides::default()
    };
    let mode = if down.is_empty() {
        mode
    } else {
        fit(mode, punktfunk_core::Mode::default(), &down, codec_max)
    };
    let caps = if o.hdr == Some(false) {
        caps & !punktfunk_core::quic::VIDEO_CAP_HDR
    } else {
        caps
    };
    (mode, caps, o.gamepad_pref().unwrap_or(pad))
}

/// Mode-conflict before Welcome: stop this client's own earlier sessions, which skip their
/// drain, then join, steal or refuse a live one; a join inside another connect's bring-up waits
/// for it ([`JOIN_WAIT`](crate::vdisplay::admission::JOIN_WAIT)). `Some` = joined: the owner's
/// display and the size this client shows it at, its kept `view` or else the mode it asked for.
/// Same-client reconnect never conflicts. The reservation is this connect's place in the live
/// set from here on: a later client conflicts with it, and a steal raises this session's `stop`.
async fn admit_mode(
    n: &Negotiation<'_>,
    hello: &mut Hello,
    codec: crate::encode::Codec,
    view: Option<(u32, u32)>,
) -> Result<(
    Option<(crate::vdisplay::admission::LiveDisplay, (u32, u32))>,
    crate::vdisplay::admission::Reservation,
)> {
    use crate::vdisplay::admission::{admit, preempt_resumed, preempt_same_identity, Admission};
    let peer_fp = n.conn.peer_fingerprint();

    // Own prior session (QUIC idle has not fired), and on `punktfunk/2` the exact session a
    // `resume` names. Supersede them and wait for their release, so this reconnect reuses the
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
        crate::vdisplay::admission::supersede(&own_zombies);
        wait_released(&own_zombies).await;
    }

    let mode = (hello.mode.width, hello.mode.height, hello.mode.refresh_hz);
    // `join` may wait out another connect's bring-up: off the runtime's workers.
    let stop = n.stop.clone();
    let (decision, reservation) = tokio::task::spawn_blocking(move || admit(peer_fp, mode, stop))
        .await
        .context("admission task")?;
    let joined = match decision {
        Admission::Separate => None,
        Admission::Join(m, display) => {
            tracing::info!(
                requested =
                    %format_args!("{}x{}@{}", hello.mode.width, hello.mode.height, hello.mode.refresh_hz),
                live = %format_args!("{}x{}@{}", m.0, m.1, m.2),
                "mode-conflict: JOIN — admitting at the live display's mode"
            );
            let view = view.unwrap_or((hello.mode.width, hello.mode.height));
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
            // not refused by it. BUSY with no reason bytes: the client reads `ApplicationClosed`
            // and words `reject_busy`; the live mode stays in the log.
            drop(reservation);
            let busy = punktfunk_core::reject::RejectReason::Busy.message_id();
            n.conn
                .refuse_said(REJECT_BUSY_CODE, &super::Said::new(&busy, ""))
                .await;
            anyhow::bail!("{reason}");
        }
    };
    Ok((joined, reservation))
}

/// `Welcome::host_caps`: what this host does for this session. `0x80` is the last bit; the next
/// capability goes in [`welcome_host_caps2`].
fn welcome_host_caps(n: &Negotiation<'_>, t: &SessionTerms) -> u8 {
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
        // KWin keysyms; gamescope's input method). Clients without the bit keep VK synthesis.
        | if crate::inject::text_input_supported() {
            punktfunk_core::quic::HOST_CAP_TEXT_INPUT
        } else {
            0
        }
        // Client turns its local renderer on only when it sees this bit; `negotiate`
        // reads the bit back into `cursor_forward` for the session's wiring.
        | if cursor_forward(
            t.hello.client_caps,
            t.compositor,
            t.codec,
            t.verdict.bit_depth,
            t.verdict.hdr,
            t.tenure.shares_capture(),
            &t.encoder,
        ) {
            punktfunk_core::quic::HOST_CAP_CURSOR
        } else {
            0
        }
        // Pen batches → per-session uinput tablet. Without the bit, clients fold pen into
        // touch/pointer and `NativeClient::send_pen` refuses.
        | if crate::inject::pen_supported() && uinput_reaches(t.compositor) {
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
        // The virtual path binds its data plane before its pipeline exists (bring-up trace:
        // `plane_bound` to `first_packet`) and serves the client's ramp in that gap. The
        // rate-following source holds its first frame back (`--bringup-ms`) to match. The
        // byte-pattern source has no such gap: video leaves at once.
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
            let game_mode = game_mode_pref(hello.launch.as_deref());
            if game_mode.is_some() {
                tracing::info!(
                    asked = hello.compositor.as_str(),
                    "gamescope compositor for the Game Mode tile"
                );
            }
            let pref = game_mode.unwrap_or(hello.compositor);
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
            // Game Mode is the box's own session, never a dedicated spawn.
            let dedicated = game_mode.is_none()
                && crate::vdisplay::wants_dedicated_game_session(
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
    // The Welcome's pen and touchpad offers read the answer. A desktop gives it in about
    // 100 ms; a headless one never does, and the connect does not wait out its two seconds.
    #[cfg(target_os = "linux")]
    if compositor.is_some_and(|c| pen_reaches(Some(c))) {
        let asked = tokio::task::spawn_blocking(crate::inject::uinput_reaches);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), asked).await;
    }
    Ok((compositor, gamescope_route))
}

/// What a Game Mode tile connect ([`crate::library::GAME_MODE_ID`]) asks for: gamescope, which
/// the routing ladder takes over where the box's own session allows, else spawns with Big
/// Picture. `None` for any other launch.
fn game_mode_pref(launch: Option<&str>) -> Option<CompositorPref> {
    (launch == Some(crate::library::GAME_MODE_ID)).then_some(CompositorPref::Gamescope)
}

/// The connector a `screen` echo names: the monitor the session mirrors.
fn mirrored(screen: Option<&ScreenEcho>) -> Option<&str> {
    match screen {
        Some(ScreenEcho::Monitor(connector)) => Some(connector.as_str()),
        _ => None,
    }
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
    /// A joiner's: whether the owner's display shows HDR.
    pub(super) owner_hdr: Option<bool>,
    /// A joiner's: the format of the capture it shares, the ceiling of its own.
    pub(super) owner_format: Option<crate::vdisplay::admission::SampleFormat>,
    /// The monitor the session mirrors, by connector. Its own HDR state bounds the stream's
    /// where the capture composes HDR from any display (macOS).
    pub(super) mirrored: Option<String>,
    /// The rule runs again when the source moves: the session reaches another compositor, or
    /// its content turns HDR or SDR. Only for a client that rebuilds its decoder at an epoch,
    /// on a capture the session does not share.
    pub(super) follows: bool,
    /// Whether the source shows HDR content now, where it says ([`Self::follows_content`]).
    /// `Some(false)` streams SDR off a display lit in HDR. `None` streams what it is lit in.
    pub(super) content_hdr: Option<bool>,
}

/// What [`FormatFacts::resolve`] settles: the stream's depth, HDR verdict and chroma, and
/// whether its display is lit in HDR, which holds while the content on it is SDR.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Verdict {
    pub(super) bit_depth: u8,
    pub(super) hdr: bool,
    pub(super) chroma: crate::encode::ChromaFormat,
    pub(super) display_hdr: bool,
}

impl FormatFacts {
    /// `owner` is the display a joiner was admitted onto. `follows` is [`follows_source`].
    /// `mirrored` is the connector of the monitor the session mirrors.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn of(
        hello: &Hello,
        studio_asked: crate::session_plan::StudioAsk,
        compositor: Option<crate::vdisplay::Compositor>,
        gamescope_route: Option<&crate::vdisplay::GamescopeRoute>,
        knobs: &SessionKnobs,
        owner: Option<&crate::vdisplay::admission::LiveDisplay>,
        follows: bool,
        mirrored: Option<&str>,
    ) -> FormatFacts {
        let owner_format = owner.and_then(|d| d.format);
        FormatFacts {
            video_caps: hello.video_caps,
            compositor,
            gamescope_route: gamescope_route.cloned(),
            sdr10_widen: knobs.sdr10_widen,
            studio_asked: asked_under(owner_format, studio_asked),
            owner_hdr: owner.map(|d| d.hdr),
            owner_format,
            mirrored: mirrored.map(str::to_owned),
            follows,
            content_hdr: None,
        }
    }

    /// Whether the monitor this session mirrors can show HDR now. macOS composes PQ from any
    /// display, so an SDR panel would stream SDR content as HDR. Elsewhere, and for a session's
    /// own screen, the capture's answer already covers it.
    fn mirror_has_hdr(&self) -> bool {
        let Some(connector) = &self.mirrored else {
            return true;
        };
        #[cfg(target_os = "macos")]
        return crate::vdisplay::display_hdr(connector);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = connector;
            true
        }
    }

    /// gamescope names the HDR state of what it shows; every other source streams what its
    /// display is lit in. A capture another session shares stays in its range: the stream asks
    /// before each move ([`crate::vdisplay::admission::move_format`]).
    pub(super) fn follows_content(&self) -> bool {
        self.follows && self.compositor == Some(crate::vdisplay::Compositor::Gamescope)
    }

    /// The facts of a session that moved to `compositor` on `route`. `content_hdr` is what the
    /// new source shows; a gamescope that just started shows SDR.
    pub(super) fn moved_to(
        &self,
        compositor: crate::vdisplay::Compositor,
        route: Option<&crate::vdisplay::GamescopeRoute>,
    ) -> FormatFacts {
        let mut moved = FormatFacts {
            compositor: Some(compositor),
            gamescope_route: route.cloned(),
            ..self.clone()
        };
        moved.content_hdr = moved.follows_content().then_some(false);
        moved
    }

    /// What this session streams a shared capture at, or what it misses of it. `resolved` is
    /// [`Self::resolve`]'s answer for `codec`; `None` for a session that shares no capture.
    pub(super) fn joined_format(
        &self,
        codec: crate::encode::Codec,
        encoder: &crate::encode::Resolved,
        (bit_depth, hdr, chroma): (u8, bool, crate::encode::ChromaFormat),
    ) -> Option<Result<(), crate::vdisplay::admission::FormatGap>> {
        let can = crate::vdisplay::admission::SampleFormat {
            bit_depth,
            chroma_444: chroma.is_444(),
            full_range: crate::session_plan::full_range_for(
                encoder,
                self.studio_asked,
                codec,
                hdr,
                bit_depth,
            ),
            hdr,
        };
        let joined = crate::vdisplay::admission::joined_format(self.owner_format?, can);
        Some(joined.map(|_| ()))
    }

    /// Bit depth, HDR verdict, and chroma for `codec` at `mode`, each gated by host, client,
    /// codec, source, and `encoder`, the session's resolution. A joiner asks HDR only of an HDR
    /// display, streams the range of a capture it shares, and lights nothing: its display is
    /// lit as its owner lit it. A source showing SDR content streams SDR and keeps its display
    /// lit in HDR. `mode` is the one the pipeline opens at: the Hello's at the handshake, the
    /// live one later. Blocks on its probes.
    pub(super) fn resolve(
        &self,
        codec: crate::encode::Codec,
        encoder: &crate::encode::Resolved,
        mode: punktfunk_core::Mode,
    ) -> Verdict {
        let (compositor, gamescope_route) = (self.compositor, self.gamescope_route.as_ref());
        // 10-bit only when host, client, codec, capture, and GPU all allow it. Resolved before
        // Welcome so `color` matches the stream (a can't-10-bit GPU yields 8-bit SDR). A shared
        // capture's depth and chroma are a joiner's ceiling: no session converts its samples.
        let shared = self.owner_format;
        let host_wants_10bit =
            pf_host_config::config().ten_bit && shared.is_none_or(|f| f.bit_depth == 10);
        let client_supports_10bit = self.video_caps & punktfunk_core::quic::VIDEO_CAP_10BIT != 0;
        // `VIDEO_CAP_HDR` is BT.2020 PQ. `VIDEO_CAP_10BIT` alone is 10-bit SDR (Main10, display
        // untouched).
        let client_wants_hdr = hdr_asked(self.video_caps, self.owner_hdr);
        // Source-aware: Linux HDR depends on the compositor just resolved. Gamescope and direct
        // capture fold in `hdr_capture_failed(VirtualOutput)`; GameStream's rtsp.rs check has no
        // twin here: that latch is per-source and this gate already used this session's source.
        let capture_supports_hdr =
            crate::capture::capturer_supports_hdr_for(compositor, gamescope_route, encoder)
                && self.mirror_has_hdr();
        // An 8-bit desktop's SDR-10 only widens: a colour pass per frame for no source detail, and
        // the operator opts in (`sdr10_widen`). Our gamescope composites 10-bit SDR, and Windows 11
        // 24H2 composes the virtual display in SDR wide colour unless this host saw it refused
        // (`sdr10_source`), with no opt-in. Linux non-PyroWave 4:4:4 SDR is clamped to 8-bit below.
        let sdr10_widen = self.sdr10_widen;
        // A Windows joiner captures through WGC, which reads an SDR display at 8 bits.
        let sdr10_source = crate::capture::capturer_delivers_sdr10_for(compositor, gamescope_route)
            && !(cfg!(windows) && self.owner_hdr.is_some());
        let sdr10_chain_ok = (sdr10_widen || sdr10_source)
            && codec_carries_sdr10(codec, sdr10_source)
            && encoder.carries_sdr10(codec);
        // The display can be lit in HDR; the stream is HDR while its content is.
        let lit_hdr = client_wants_hdr && capture_supports_hdr;
        let source_hdr = stream_hdr(lit_hdr, self.content_hdr, shared);
        let depth_reachable = source_hdr || sdr10_chain_ok;
        // Probe may open a tiny encoder: short-circuited behind the cheap gates.
        let ten_bit_asked = host_wants_10bit && client_supports_10bit && codec.supports_10bit();
        let (gpu_can_10bit, display_hdr) =
            ten_bit_gates(ten_bit_asked, depth_reachable, lit_hdr, || {
                encoder.can_encode_10bit(codec)
            });
        // A request lit otherwise finds no display to join.
        let display_hdr = self.owner_hdr.unwrap_or(display_hdr);
        let bit_depth: u8 = if gpu_can_10bit { 10 } else { 8 };
        // The colour label and the capturer mandate read this, never `bit_depth >= 10` (10-bit
        // without it is SDR: BT.709 VUI).
        let session_hdr = gpu_can_10bit && source_hdr;
        tracing::info!(
            bit_depth,
            session_hdr,
            display_hdr,
            content_hdr = ?self.content_hdr,
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
        let host_wants_444 =
            pf_host_config::config().four_four_four && shared.is_none_or(|f| f.chroma_444);
        let client_supports_444 = self.video_caps & punktfunk_core::quic::VIDEO_CAP_444 != 0;
        // Ingest chain, not capturer: Windows 4:4:4 needs direct-NVENC RGB ingest (AMF cannot;
        // QSV/ffmpeg has no RGB 4:4:4 wiring). PyroWave does its own RGB→YCbCr; its gate is
        // `can_encode_444`. HDR does not cost chroma (HEVC Main 4:4:4 10).
        let ingest_chain_supports_444 = codec == crate::encode::Codec::PyroWave
            || crate::capture::capturer_supports_444(encoder.ingests_rgb_444());
        // macOS capture has no 10-bit full-chroma format, so a 10-bit session there stays 4:2:0.
        let mac_ten_bit = cfg!(target_os = "macos") && bit_depth == 10;
        let ingest_chain_supports_444 = ingest_chain_supports_444 && !mac_ten_bit;
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
            } else if mac_ten_bit {
                "a 10-bit session on macOS stays 4:2:0 — capture has no 10-bit full-chroma format"
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

        Verdict {
            bit_depth,
            hdr: session_hdr,
            chroma,
            display_hdr,
        }
    }
}

/// The range ask of a session on a `shared` capture: limited where the capture is, whatever
/// the client asked. A full-range capture leaves the client's ask, which then refuses it.
fn asked_under(
    shared: Option<crate::vdisplay::admission::SampleFormat>,
    asked: crate::session_plan::StudioAsk,
) -> crate::session_plan::StudioAsk {
    match shared {
        Some(f) if !f.full_range => crate::session_plan::StudioAsk {
            sdr: true,
            hdr: true,
        },
        _ => asked,
    }
}

/// Whether a session's format may move with its source ([`FormatFacts::follows`]): the client
/// rebuilds its decoder at an epoch, and the capture is the session's own.
pub(super) fn follows_source(
    features: &punktfunk_core::quic::v2::features::FeatureSet,
    tenure: &Tenure,
) -> bool {
    features.has(punktfunk_core::quic::v2::registry::FEATURE_FORMAT_EPOCH)
        && !tenure.shares_capture()
}

/// HDR the client asks for: `VIDEO_CAP_HDR`, except a joiner of an SDR display (`owner_hdr`).
fn hdr_asked(video_caps: u8, owner_hdr: Option<bool>) -> bool {
    video_caps & punktfunk_core::quic::VIDEO_CAP_HDR != 0 && owner_hdr != Some(false)
}

/// Whether the stream off a display that can be lit in HDR (`lit_hdr`) is HDR: not while its
/// content is SDR, nor for a joiner of a `shared` capture that hands out SDR.
fn stream_hdr(
    lit_hdr: bool,
    content_hdr: Option<bool>,
    shared: Option<crate::vdisplay::admission::SampleFormat>,
) -> bool {
    lit_hdr && content_hdr != Some(false) && shared.is_none_or(|f| f.hdr)
}

/// The GPU's 10-bit verdict for the stream (`depth_reachable`) and for the lit display
/// (`lit_hdr`), from one ask: two could answer differently while a probe is unanswered. The ask
/// runs only when a gate that needs it is open.
fn ten_bit_gates(
    ten_bit_asked: bool,
    depth_reachable: bool,
    lit_hdr: bool,
    gpu_can_10bit: impl FnOnce() -> bool,
) -> (bool, bool) {
    let can = ten_bit_asked && (depth_reachable || lit_hdr) && gpu_can_10bit();
    (depth_reachable && can, lit_hdr && can)
}

/// [`FormatFacts::resolve`] off the runtime: its probes may open a tiny encoder. A source that
/// names its content is read first; a gamescope not up yet shows SDR.
async fn negotiate_video_format(
    mut facts: FormatFacts,
    codec: crate::encode::Codec,
    encoder: Arc<crate::encode::Resolved>,
    mode: punktfunk_core::Mode,
) -> Result<Verdict> {
    tokio::task::spawn_blocking(move || {
        if facts.follows_content() {
            facts.content_hdr = Some(crate::vdisplay::hdr_content_now(None));
        }
        facts.resolve(codec, &encoder, mode)
    })
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
/// that pair (or a bare zero) is "nothing asked".
fn audio_format_asked(rate_hz: u32, bits: u8) -> bool {
    use punktfunk_core::audio::{pcm, SAMPLE_RATE_HZ};
    (rate_hz != 0 && rate_hz != SAMPLE_RATE_HZ) || (bits != 0 && bits != pcm::BITS_16)
}

/// Cap on waiting for MTU discovery to reach the datagram a PCM frame needs. A connection
/// starts at 1 200 bytes and climbs one acked probe per round trip.
const PCM_MTU_WAIT: std::time::Duration = std::time::Duration::from_millis(300);
const PCM_MTU_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// True when a PCM frame fits no rung in `now` bytes but one after the datagram has grown
/// by `headroom`.
fn pcm_awaits_mtu(rate_hz: u32, bits: u8, channels: u8, now: usize, headroom: usize) -> bool {
    use punktfunk_core::audio::pcm;
    pcm::frame_us_for(rate_hz, bits, channels, now).is_none()
        && pcm::frame_us_for(rate_hz, bits, channels, now + headroom).is_some()
}

/// The datagram size the PCM frame is sized from. The frame length is a Welcome promise, so
/// a format whose shortest frame fits only a climbed MTU waits for the climb, at most
/// [`PCM_MTU_WAIT`]: 48 kHz/24-bit 7.1 needs 1 165 bytes and a new connection carries 1 161.
/// A format that fits now, or never will, does not wait. quinn raises the MTU only on an
/// acked probe, so the size returned is one the path carried.
async fn pcm_datagram(
    conn: &super::link::SessionLink,
    rate_hz: u32,
    bits: u8,
    channels: u8,
) -> Option<usize> {
    use punktfunk_core::audio::pcm;
    let mut now = conn.max_datagram_size()?;
    // The datagram grows byte for byte with the MTU, up to the discovery ceiling.
    let headroom = punktfunk_core::config::video_datagram_udp_ceiling()
        .saturating_sub(usize::from(conn.current_mtu()));
    if !pcm_awaits_mtu(rate_hz, bits, channels, now, headroom) {
        return Some(now);
    }
    let t0 = std::time::Instant::now();
    while t0.elapsed() < PCM_MTU_WAIT && pcm::frame_us_for(rate_hz, bits, channels, now).is_none() {
        tokio::time::sleep(PCM_MTU_POLL).await;
        now = conn.max_datagram_size()?;
    }
    tracing::debug!(
        waited_ms = t0.elapsed().as_millis() as u64,
        max_datagram = now,
        "audio: waited for MTU discovery to carry the lossless frame"
    );
    Some(now)
}

/// Audio plane for Welcome and the channel count it codes: AC-3 / E-AC-3 passthrough when the
/// client's sink asked and [`resolve_bitstream_plane`] holds; else Opus, or lossless PCM when
/// client, operator, and the capture-rate probe all allow it. Each in the coupling the client
/// asked for when this host knows it. Async for that blocking probe and [`pcm_datagram`].
async fn negotiate_audio_plane(
    n: &Negotiation<'_>,
    hello: &Hello,
    audio_channels: u8,
    bitrate_kbps: u32,
) -> Result<(AudioPlane, u8)> {
    // quinn's current value: MTU discovery has not settled at Welcome. Passthrough only needs
    // datagrams to exist; the PCM plane sizes its frame from [`pcm_datagram`] below.
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
    let max_datagram = if hires_asked && hires_allowed {
        pcm_datagram(
            n.conn,
            hello.audio_rate_hz,
            hello.audio_bits,
            audio_channels,
        )
        .await
    } else {
        max_datagram
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
        pcm_parts(n),
    );
    Ok((AudioPlane { layout, ..plane }, audio_channels))
}

/// Whether this session may send a PCM frame as `0xD5` parts: the client joins them, and
/// the carrier reports its datagram buffer space, which the browser plane does not.
fn pcm_parts(n: &Negotiation<'_>) -> bool {
    !n.conn.is_web()
        && n.first
            .features
            .has(punktfunk_core::quic::v2::registry::FEATURE_PCM_PARTS)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use punktfunk_core::audio::pcm;

    /// A stopped session's wait ends when its row leaves the live set, and at the cap when it
    /// never does. The test sets the cap: a minute where the release has to win, 50 ms where
    /// nothing leaves.
    #[tokio::test]
    async fn a_release_ends_the_wait_and_the_cap_ends_a_stuck_one() {
        use crate::vdisplay::admission::{all_gone, register};
        let ms = std::time::Duration::from_millis;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let stops = std::slice::from_ref(&stop);
        // No identity and a mode no session asks for: no other test's admission reads it.
        let row = || {
            register(
                None,
                (1, 1, 1),
                stop.clone(),
                "a test".into(),
                Default::default(),
                None,
            )
        };

        let leaves = row();
        let leaving = tokio::spawn(async move {
            tokio::time::sleep(ms(20)).await;
            drop(leaves);
        });
        let (asked, cap) = (std::time::Instant::now(), ms(60_000));
        wait_released_within(stops, cap).await;
        assert!(
            all_gone(stops) && asked.elapsed() < cap,
            "the release ends it"
        );
        leaving.await.unwrap();

        let stays = row();
        let (asked, cap) = (std::time::Instant::now(), ms(50));
        wait_released_within(stops, cap).await;
        assert!(
            !all_gone(stops) && asked.elapsed() >= cap,
            "the cap ends it"
        );
        drop(stays);
    }

    /// A session keeps the fraction it asked for where its own display runs it, and loses one
    /// that names another rate. A whole-hertz backend, a shared display and a Windows host
    /// answer whole hertz.
    #[test]
    fn a_session_streams_at_a_rate_the_host_delivers() {
        use crate::vdisplay::Compositor::{Hyprland, Kwin, Mutter, Niri, Umbriel, Wlroots};
        let asked = punktfunk_core::Mode::exact(1920, 1080, 59_940);
        let whole = punktfunk_core::Mode::exact(1920, 1080, 60_000);
        assert_eq!(deliverable_rate(asked, true), asked);
        assert_eq!(deliverable_rate(asked, false), whole);
        // A cap moved 120 Hz down to 60: 119.88 is no fraction of that.
        let mut capped = punktfunk_core::Mode::exact(1920, 1080, 119_880);
        capped.refresh_hz = 60;
        assert_eq!(deliverable_rate(capped, true), whole);
        assert_eq!(deliverable_rate(whole, true), whole);

        for c in [Wlroots, Hyprland, Mutter, Kwin, Niri, Umbriel] {
            assert!(runs_exact_rate(Some(c), false), "{c:?}");
            assert!(!runs_exact_rate(Some(c), true), "{c:?} shared");
        }
        assert!(runs_exact_rate(None, false));
    }

    /// A Mac forwards the pointer to a client that asked, a mirror or a joiner too, and only
    /// while the system hands out the pointer's image.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_mac_offers_the_pointer_only_when_it_can_leave_it_out() {
        let encoder = crate::encode::resolve();
        let asks = punktfunk_core::quic::CLIENT_CAP_CURSOR;
        let offer = |caps, worker| {
            cursor_forward(
                caps,
                None,
                crate::encode::Codec::H265,
                8,
                false,
                worker,
                &encoder,
            )
        };
        assert!(!offer(0, false), "a client that did not ask");
        let readable = pf_capture::pointer_image_readable();
        assert_eq!(offer(asks, false), readable);
        assert_eq!(offer(asks, true), readable, "a mirror's stream is its own");
    }

    /// A rate stored for PyroWave reads as Automatic once the host lands on HEVC: the session
    /// resolves the host's default, and every later `== 0` test (the governor, a rebuild's
    /// re-resolve) sees Automatic, as the client does.
    #[test]
    fn a_pyrowave_rate_on_an_hevc_session_is_automatic() {
        use punktfunk_core::quic::{CODEC_HEVC, CODEC_PYROWAVE};
        let mut hello = Hello {
            bitrate_kbps: 80_000,
            video_codecs: CODEC_HEVC | CODEC_PYROWAVE,
            preferred_codec: CODEC_PYROWAVE,
            ..hello_at(mode(1920, 1080, 60))
        };
        let mut on_pyrowave = hello.clone();
        settle_bitrate(&mut on_pyrowave, CODEC_PYROWAVE);
        assert_eq!(on_pyrowave.bitrate_kbps, 80_000, "Fixed stays Fixed");
        settle_bitrate(&mut hello, CODEC_HEVC);
        assert_eq!(hello.bitrate_kbps, 0);
        let picture = Intent::of(hello.mode, None).picture();
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::H265,
                hello.bitrate_kbps,
                &picture,
                crate::encode::ChromaFormat::Yuv420,
                8,
                session_pyrowave_bpp(0),
                None,
            ),
            20_000
        );
    }

    pub(in crate::native) fn mode(
        width: u32,
        height: u32,
        refresh_hz: u32,
    ) -> punktfunk_core::Mode {
        punktfunk_core::Mode {
            width,
            height,
            refresh_hz,
            refresh_mhz: 0,
        }
    }

    /// A client's `Hello` for `mode`, at Automatic, offering HEVC alone.
    pub(in crate::native) fn hello_at(mode: punktfunk_core::Mode) -> Hello {
        use punktfunk_core::config::{CompositorPref, GamepadPref};
        Hello {
            mode,
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: punktfunk_core::quic::CODEC_HEVC,
            preferred_codec: punktfunk_core::quic::CODEC_HEVC,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: punktfunk_core::audio::SAMPLE_RATE_HZ,
            audio_bits: pcm::BITS_16,
            audio_layout: 0,
            video_fit: 0,
            audio_bitstreams: 0,
        }
    }

    /// The terms of an HEVC session on Mutter for `hello`: 8-bit SDR 4:2:0, a display of its
    /// own, no view and no mirror. A test sets the fields it exercises.
    pub(in crate::native) fn terms_for(
        hello: Hello,
        encoder: Arc<crate::encode::Resolved>,
    ) -> SessionTerms {
        SessionTerms {
            intent: Intent::of(hello.mode, None),
            hello,
            codec_bit: punktfunk_core::quic::CODEC_HEVC,
            codec: crate::encode::Codec::H265,
            tenure: Tenure::of(None, false),
            joined: None,
            compositor: Some(crate::vdisplay::Compositor::Mutter),
            gamescope_route: None,
            encoder,
            gamepad: GamepadPref::Auto,
            audio_channels: 2,
            verdict: Verdict {
                bit_depth: 8,
                hdr: false,
                chroma: crate::encode::ChromaFormat::Yuv420,
                display_hdr: false,
            },
            studio_asked: Default::default(),
            pyrowave_bpp: 1.6,
            pyrowave_max_kbps: None,
            slices: 1,
            bitrate_kbps: 20_000,
            shard_payload: 1200,
            chacha: false,
            audio_plane: AudioPlane::opus(),
            cursor_forward: false,
            audio_kbps: 0,
            screen: None,
            wanted: None,
            topology: None,
            role: None,
            display_mm: None,
        }
    }

    /// The display a joiner was admitted onto: Mutter's, lit in SDR, its format not known.
    pub(in crate::native) fn live_display() -> crate::vdisplay::admission::LiveDisplay {
        crate::vdisplay::admission::LiveDisplay {
            compositor: Some(crate::vdisplay::Compositor::Mutter),
            route: None,
            isolation: None,
            audio_sink: Arc::default(),
            hdr: false,
            format: None,
        }
    }

    /// A view is kept only as a size a mode could be, inside the mode and smaller on one axis.
    #[test]
    fn a_view_is_kept_only_inside_its_mode() {
        use crate::encode::Codec::{PyroWave, H265};
        let at_1_5x = mode(2048, 1152, 120);
        let own = Tenure::of(None, false);
        let view_at = |view, mode, codec| Intent::at_hello(mode, view, &own, codec).view();
        let kept = |view, codec| view_at(Some(view), at_1_5x, codec);
        assert_eq!(kept((1366, 768), H265), Some((1366, 768)), "fine");
        assert_eq!(
            kept((2048, 768), H265),
            Some((2048, 768)),
            "smaller on one axis"
        );
        assert_eq!(kept((2048, 1152), H265), None, "equal: the mode");
        assert_eq!(kept((2560, 1440), H265), None, "too large");
        assert_eq!(kept((1366, 1440), H265), None, "too large on one axis");
        assert_eq!(kept((1367, 768), H265), None, "odd");
        assert_eq!(kept((0, 768), H265), None, "zero");
        assert_eq!(kept((120, 100), PyroWave), None, "under the codec's floor");
        assert_eq!(view_at(None, at_1_5x, H265), None);
    }

    /// A joiner frames for its own view at its fit, whatever view the client also named; any
    /// other session for its view at Fit, or not at all.
    #[test]
    fn a_joiner_frames_for_its_view_and_a_viewer_at_fit() {
        use crate::encode::Codec::H265;
        use punktfunk_core::video_fit::VideoFit;
        let crop = VideoFit::from_wire(VideoFit::Crop.wire());
        let hd = mode(1920, 1080, 60);
        let framing = |view, tenure| Intent::at_hello(hd, view, &tenure, H265).framing();
        let joiner = Tenure::of(Some((crop, (1280, 720))), false);
        assert_eq!(
            framing(Some((1024, 576)), joiner),
            Some((VideoFit::Crop, (1280, 720)))
        );
        let own = Tenure::of(None, false);
        assert_eq!(
            framing(Some((1366, 768)), own),
            Some((VideoFit::Fit, (1366, 768)))
        );
        assert_eq!(framing(None, own), None);
    }

    /// Where a Welcome's session streams: a display of its own, one capped by the operator to a
    /// mode, a display another session runs at its mode, or a host monitor.
    #[derive(Clone, Copy, Debug)]
    pub(in crate::native) enum Welcomed {
        Own,
        Capped(punktfunk_core::Mode),
        Joiner(punktfunk_core::Mode),
        Mirror,
    }

    /// The Welcome's mode, rate and `ServerHello` picture for a client that asked `asked` at
    /// `view` and `kbps`, through `negotiate`'s steps in its order on a mutter host. The cap
    /// and the join enter as the mode they leave; PyroWave runs at 1.6 bits per pixel.
    pub(in crate::native) fn welcomed(
        asked: punktfunk_core::Mode,
        view: Option<(u32, u32)>,
        codec: crate::encode::Codec,
        kbps: u32,
        at: Welcomed,
    ) -> (punktfunk_core::Mode, u32, Option<(u32, u32)>) {
        let mut mode = asked;
        if let Welcomed::Capped(cap) = at {
            (mode.width, mode.height, mode.refresh_hz) = (cap.width, cap.height, cap.refresh_hz);
        }
        let asked = Intent::at_hello(mode, view, &Tenure::of(None, false), codec);
        let joined = match at {
            Welcomed::Joiner(owner) => {
                let shown = asked.view().unwrap_or((mode.width, mode.height));
                (mode.width, mode.height, mode.refresh_hz) =
                    (owner.width, owner.height, owner.refresh_hz);
                Some((punktfunk_core::video_fit::VideoFit::Fit, shown))
            }
            _ => None,
        };
        let tenure = Tenure::of(joined, matches!(at, Welcomed::Mirror));
        let exact = runs_exact_rate(
            Some(crate::vdisplay::Compositor::Mutter),
            tenure.shares_capture(),
        );
        let mode = deliverable_rate(mode, exact);
        let intent = Intent::at_hello(mode, asked.view(), &tenure, codec);
        let kbps = resolve_bitrate_kbps_for(
            codec,
            kbps,
            &intent.picture(),
            crate::encode::ChromaFormat::Yuv420,
            8,
            session_pyrowave_bpp(160),
            None,
        );
        (mode, kbps, intent.picture().wire(mode))
    }

    /// One Hello and the Welcome it gets.
    struct WelcomeRow {
        name: &'static str,
        asked: punktfunk_core::Mode,
        view: Option<(u32, u32)>,
        codec: crate::encode::Codec,
        kbps: u32,
        at: Welcomed,
        welcome: (punktfunk_core::Mode, u32, Option<(u32, u32)>),
        verdict: crate::native::tests::Verdict,
    }

    #[rustfmt::skip]
    fn welcome_rows() -> Vec<WelcomeRow> {
        use crate::encode::Codec::{PyroWave, H265};
        use crate::native::tests::Verdict::Wanted;
        let (hd, qhd, uhd) = (mode(1920, 1080, 60), mode(2560, 1440, 60), mode(3840, 2160, 60));
        let row = |name, asked, view, codec, kbps, at, welcome, verdict| WelcomeRow {
            name,
            asked,
            view,
            codec,
            kbps,
            at,
            welcome,
            verdict,
        };
        let exact = punktfunk_core::Mode::exact(1920, 1080, 59_940);
        vec![
            row("HEVC Automatic: the host's default", hd, None, H265, 0, Welcomed::Own,
                (hd, 20_000, None), Wanted),
            row("HEVC Automatic with a view: not priced by size, told the view", uhd,
                Some((1920, 1080)), H265, 0, Welcomed::Own, (uhd, 20_000, Some((1920, 1080))),
                Wanted),
            row("HEVC at a fixed rate: the rate stands", uhd, Some((1920, 1080)), H265, 50_000,
                Welcomed::Own, (uhd, 50_000, Some((1920, 1080))), Wanted),
            row("PyroWave at the mode", hd, None, PyroWave, 0, Welcomed::Own,
                (hd, 199_065, None), Wanted),
            row("PyroWave priced and told at its view", uhd, Some((1920, 1080)), PyroWave, 0,
                Welcomed::Own, (uhd, 199_065, Some((1920, 1080))), Wanted),
            row("PyroWave Fixed: the pin at the view, the client's number ignored", uhd,
                Some((1920, 1080)), PyroWave, 80_000, Welcomed::Own,
                (uhd, 199_065, Some((1920, 1080))), Wanted),
            row("a view the mode's size: the mode", hd, Some((1920, 1080)), PyroWave, 0,
                Welcomed::Own, (hd, 199_065, None), Wanted),
            row("a view larger than the mode: the mode", hd, Some((2560, 1440)), PyroWave, 0,
                Welcomed::Own, (hd, 199_065, None), Wanted),
            row("a fraction on a display of its own stays", exact, None, H265, 0, Welcomed::Own,
                (exact, 20_000, None), Wanted),
            row("a joiner asking less: the owner's mode, priced and told at its ask",
                mode(1280, 720, 120), None, PyroWave, 0, Welcomed::Joiner(hd),
                (hd, 88_473, Some((1280, 720))), Wanted),
            row("a joiner with a view: the owner's mode at that view", uhd, Some((1920, 1080)),
                PyroWave, 0, Welcomed::Joiner(qhd), (qhd, 199_065, Some((1920, 1080))), Wanted),
            row("a joiner at the owner's size: the mode, whole hertz", exact, None, PyroWave, 0,
                Welcomed::Joiner(hd), (hd, 199_065, None), Wanted),
            row("a joiner asking more than the owner runs: priced and told at the mode its fit \
                 encodes", uhd, None, PyroWave, 0, Welcomed::Joiner(hd), (hd, 199_065, None),
                Wanted),
            row("an HEVC joiner asking more: the default, told the mode", uhd, None, H265, 0,
                Welcomed::Joiner(hd), (hd, 20_000, None), Wanted),
            row("a mirror: its view dropped, priced and told at the mode", hd,
                Some((1280, 720)), PyroWave, 0, Welcomed::Mirror, (hd, 199_065, None), Wanted),
            row("a cap keeps a view inside the capped mode", mode(3840, 2160, 120),
                Some((1920, 1080)), PyroWave, 0, Welcomed::Capped(qhd),
                (qhd, 199_065, Some((1920, 1080))), Wanted),
            row("a cap to the view's size: the mode", uhd, Some((1920, 1080)), PyroWave, 0,
                Welcomed::Capped(hd), (hd, 199_065, None), Wanted),
            row("a cap under the view: the capped mode", uhd, Some((1920, 1080)), PyroWave, 0,
                Welcomed::Capped(mode(1280, 720, 60)), (mode(1280, 720, 60), 88_473, None),
                Wanted),
        ]
    }

    /// The Welcome's mode, rate and picture for each Hello shape: HEVC and PyroWave, with and
    /// without a view, as a joiner, as a mirror and under a cap.
    #[test]
    fn geometry_rows() {
        use crate::encode::Codec::PyroWave;
        // A host built without the PyroWave encoder takes no PyroWave size, nor its view.
        let pyrowave = crate::encode::validate_dimensions(PyroWave, 1920, 1080).is_ok();
        for r in welcome_rows() {
            if r.codec == PyroWave && !pyrowave {
                continue;
            }
            let got = welcomed(r.asked, r.view, r.codec, r.kbps, r.at);
            assert_eq!(got, r.welcome, "{} ({:?}, {:?})", r.name, r.at, r.verdict);
        }
    }

    /// A joiner's colour is the owner display's: an SDR display asks no HDR of an HDR client,
    /// an HDR one keeps an HDR client's ask, and a client that can't ask HDR of it is refused.
    /// The verdict stands in for a GPU and capture that allow whatever is asked.
    #[test]
    fn a_joiner_takes_the_owners_colour_or_is_refused() {
        use crate::vdisplay::admission::joined_hdr;
        use punktfunk_core::quic::{VIDEO_CAP_10BIT, VIDEO_CAP_HDR};
        let hdr_client = VIDEO_CAP_10BIT | VIDEO_CAP_HDR;
        let join = |caps, owner| joined_hdr(owner, hdr_asked(caps, Some(owner)));
        assert_eq!(
            join(hdr_client, false),
            Some(false),
            "SDR display, HDR client"
        );
        assert_eq!(
            join(hdr_client, true),
            Some(true),
            "HDR display, HDR client"
        );
        assert_eq!(join(VIDEO_CAP_10BIT, true), None, "HDR display, SDR client");
        assert!(hdr_asked(hdr_client, None), "not a joiner: its own ask");
    }

    /// A display lit in HDR streams SDR while its content is SDR, to its owner and to a joiner
    /// of its capture alike. A display that is not lit streams SDR whatever it shows.
    #[test]
    fn a_lit_display_streams_the_range_of_its_content() {
        use crate::vdisplay::admission::SampleFormat;
        let shared = |hdr| {
            Some(SampleFormat {
                bit_depth: 10,
                chroma_444: false,
                full_range: true,
                hdr,
            })
        };
        assert!(stream_hdr(true, None, None), "no content named: as lit");
        assert!(stream_hdr(true, Some(true), None));
        assert!(!stream_hdr(true, Some(false), None), "SDR content");
        assert!(stream_hdr(true, None, shared(true)), "a joiner of PQ");
        assert!(!stream_hdr(true, None, shared(false)), "a joiner of SDR");
        assert!(!stream_hdr(false, Some(true), shared(true)), "not lit");
    }

    /// A joiner of a limited-range capture streams limited range; a full-range one leaves its
    /// ask alone, and a session with no shared capture keeps its own.
    #[test]
    fn a_joiner_asks_a_shared_captures_range() {
        use crate::session_plan::StudioAsk;
        use crate::vdisplay::admission::SampleFormat;
        let shared = |full_range| {
            Some(SampleFormat {
                bit_depth: 8,
                chroma_444: false,
                full_range,
                hdr: false,
            })
        };
        let (none, both) = (
            StudioAsk::default(),
            StudioAsk {
                sdr: true,
                hdr: true,
            },
        );
        assert_eq!(asked_under(shared(false), none), both);
        assert_eq!(asked_under(shared(true), both), both);
        assert_eq!(asked_under(shared(true), none), none);
        assert_eq!(asked_under(None, none), none);
    }

    /// The host lowers what a client asked for and never raises it: it knows no panel, so a
    /// 4K rule on a 1080p request is the client's to apply.
    #[test]
    fn a_game_override_only_lowers_the_mode() {
        use punktfunk_core::game_overrides::GameOverrides;
        let held =
            |m, o: &GameOverrides, max| game_override_terms((m, 0, GamepadPref::Auto), o, max).0;
        let o = GameOverrides {
            lines: Some(1080),
            hz: Some(60),
            ..Default::default()
        };
        assert_eq!(held(mode(2560, 1600, 120), &o, 8192), mode(1728, 1080, 60));
        assert_eq!(held(mode(3440, 1440, 144), &o, 8192), mode(2580, 1080, 60));
        assert_eq!(held(mode(1280, 800, 90), &o, 8192), mode(1280, 800, 60));
        assert_eq!(held(mode(1920, 1080, 30), &o, 8192), mode(1920, 1080, 30));
        let up = GameOverrides {
            lines: Some(2160),
            hz: Some(144),
            beyond_screen: true,
            ..Default::default()
        };
        assert_eq!(held(mode(1920, 1080, 60), &up, 8192), mode(1920, 1080, 60));
        let four_k = GameOverrides {
            lines: Some(2160),
            ..Default::default()
        };
        assert_eq!(
            held(mode(7680, 4320, 60), &four_k, 4096),
            mode(3840, 2160, 60)
        );
    }

    /// HDR off clears only the HDR bit, so 10-bit SDR stays the host's own call.
    #[test]
    fn a_game_override_turns_hdr_off_and_names_the_pad() {
        use punktfunk_core::game_overrides::GameOverrides;
        use punktfunk_core::quic::{VIDEO_CAP_10BIT, VIDEO_CAP_HDR};
        let m = mode(1920, 1080, 60);
        let asked = (m, VIDEO_CAP_10BIT | VIDEO_CAP_HDR, GamepadPref::Xbox360);
        let o = GameOverrides {
            hdr: Some(false),
            gamepad: Some("dualsense".into()),
            ..Default::default()
        };
        assert_eq!(
            game_override_terms(asked, &o, 8192),
            (m, VIDEO_CAP_10BIT, GamepadPref::DualSense)
        );
        let on = GameOverrides {
            hdr: Some(true),
            ..Default::default()
        };
        let sdr = (m, 0, GamepadPref::Xbox360);
        assert_eq!(
            game_override_terms(sdr, &on, 8192),
            sdr,
            "HDR on is the client's ask"
        );
        assert_eq!(
            game_override_terms(asked, &GameOverrides::default(), 8192),
            asked
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

    /// The 10-bit verdict is asked once for both gates, and only when a gate that needs it is
    /// open.
    #[test]
    fn the_10_bit_verdict_is_asked_once() {
        let asks = std::cell::Cell::new(0u32);
        let gate = |asked, depth, lit, answer| {
            asks.set(0);
            let gates = ten_bit_gates(asked, depth, lit, || {
                asks.set(asks.get() + 1);
                answer
            });
            (gates, asks.get())
        };
        assert_eq!(gate(true, true, true, true), ((true, true), 1));
        assert_eq!(gate(true, true, true, false), ((false, false), 1));
        assert_eq!(gate(true, true, false, true), ((true, false), 1));
        assert_eq!(gate(true, false, true, true), ((false, true), 1));
        assert_eq!(gate(true, false, false, true), ((false, false), 0));
        assert_eq!(gate(false, true, true, true), ((false, false), 0));
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

    /// The Game Mode tile asks for gamescope whatever the device prefers; no other launch does.
    #[test]
    fn the_game_mode_tile_asks_for_gamescope() {
        let tile = Some(crate::library::GAME_MODE_ID);
        assert_eq!(game_mode_pref(tile), Some(CompositorPref::Gamescope));
        assert_eq!(game_mode_pref(Some("steam:570")), None);
        assert_eq!(game_mode_pref(None), None);
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

    /// The touchpad goes where the pen does, and taps never without the pad.
    #[test]
    fn touchpad_offer_follows_the_pen_and_the_backend() {
        use crate::vdisplay::Compositor;
        use punktfunk_core::quic::v2::registry::{FEATURE_TOUCHPAD, FEATURE_TOUCHPAD_TAPS};
        let kwin = touchpad_offer(Some(Compositor::Kwin), true, true);
        assert!(kwin.has(FEATURE_TOUCHPAD) && kwin.has(FEATURE_TOUCHPAD_TAPS));
        let no_taps = touchpad_offer(None, true, false);
        assert!(no_taps.has(FEATURE_TOUCHPAD) && !no_taps.has(FEATURE_TOUCHPAD_TAPS));
        let empty = punktfunk_core::quic::v2::features::FeatureSet::default();
        assert_eq!(
            touchpad_offer(Some(Compositor::Gamescope), true, true),
            empty
        );
        assert_eq!(
            touchpad_offer(Some(Compositor::Windows), false, true),
            empty
        );
    }

    /// The negotiation line names the preference, and a preference that lost says which
    /// side lacks it — a 0x0b client against a 0x0f host, and the reverse.
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
                false,
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
            false,
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
            false,
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
                false,
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

    /// A client that joins parts gets the 24-bit surround one datagram cannot hold. One that
    /// does not keeps Opus, and a format that fits one datagram keeps its one-datagram rung.
    #[test]
    fn a_client_that_joins_parts_gets_surround_above_48_khz() {
        let resolve = |rate, ch, parts| {
            resolve_audio_plane(
                true,
                true,
                rate,
                pcm::BITS_24,
                ch,
                HONEST_CAPTURE,
                HUGE_LINK_KBPS,
                Some(DGRAM),
                parts,
            )
        };
        for (rate, ch, want_us) in [
            (88_200, 6, Some(2000)),
            (88_200, 8, Some(1500)),
            (96_000, 6, Some(2000)),
            (96_000, 8, Some(1500)),
            (176_400, 6, Some(1000)),
            // Over the datagram send buffer at the shortest rung.
            (176_400, 8, None),
        ] {
            assert_eq!(
                resolve(rate, ch, false),
                AudioPlane::opus(),
                "{rate} {ch}ch"
            );
            let p = resolve(rate, ch, true);
            match want_us {
                Some(us) => {
                    assert_eq!((p.is_pcm(), p.rate_hz, p.frame_us), (true, rate, us));
                    assert!(
                        pcm::frame_payload_bytes(rate, pcm::BITS_24, ch, us as u32) > DGRAM,
                        "{rate} {ch}ch is a frame in parts"
                    );
                }
                None => assert_eq!(p, AudioPlane::opus(), "{rate} {ch}ch"),
            }
        }
        // One datagram first: 48 kHz 7.1 and 96 kHz stereo are the same plane either way.
        for (rate, ch) in [(48_000, 8), (96_000, 2), (48_000, 6)] {
            assert_eq!(resolve(rate, ch, true), resolve(rate, ch, false));
            assert!(resolve(rate, ch, true).is_pcm());
        }
    }

    /// Only a format in the gap between a new connection's datagram and the discovered one
    /// waits. 1 161 bytes at connect, 272 more by the 1 472-byte ceiling.
    #[test]
    fn only_a_frame_mtu_discovery_can_reach_waits_for_it() {
        const NEW: usize = 1161;
        const CLIMB: usize = 272;
        assert!(pcm_awaits_mtu(48_000, pcm::BITS_24, 8, NEW, CLIMB));
        // Fits now: 5.1 takes the 1 ms rung, stereo a longer one.
        assert!(!pcm_awaits_mtu(48_000, pcm::BITS_24, 6, NEW, CLIMB));
        assert!(!pcm_awaits_mtu(96_000, pcm::BITS_24, 2, NEW, CLIMB));
        // Never fits: nothing to wait for.
        assert!(!pcm_awaits_mtu(96_000, pcm::BITS_24, 6, NEW, CLIMB));
        // Already climbed.
        assert!(!pcm_awaits_mtu(48_000, pcm::BITS_24, 8, NEW + CLIMB, 0));
        // The climbed size resolves the plane the wait was for.
        let plane = resolve_audio_plane(
            true,
            true,
            48_000,
            pcm::BITS_24,
            8,
            HONEST_CAPTURE,
            HUGE_LINK_KBPS,
            Some(NEW + CLIMB),
            false,
        );
        assert_eq!((plane.is_pcm(), plane.frame_us), (true, 1000));
        let declined = resolve_audio_plane(
            true,
            true,
            48_000,
            pcm::BITS_24,
            8,
            HONEST_CAPTURE,
            HUGE_LINK_KBPS,
            Some(NEW),
            false,
        );
        assert_eq!(declined, AudioPlane::opus());
    }

    /// A connection asked the moment it opens, 4 bytes short of 48 kHz/24-bit 7.1, waits into
    /// a datagram that carries it. Discovery needs no traffic of ours.
    #[tokio::test]
    async fn a_connection_waits_into_the_frame_it_needs() {
        use punktfunk_core::quic::endpoint;
        let server = endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            (server, conn)
        });
        let client = endpoint::client_insecure().unwrap();
        let _client_conn = client.connect(addr, "punktfunk").unwrap().await.unwrap();
        let (_server, conn) = accept.await.unwrap();
        let socket = Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
        let link = super::super::link::SessionLink::QuicV2(
            conn.clone(),
            Arc::new(super::super::link::V2Link::new(conn, socket)),
        );
        let got = pcm_datagram(&link, 48_000, pcm::BITS_24, 8).await;
        let fits = got.and_then(|d| pcm::frame_us_for(48_000, pcm::BITS_24, 8, d));
        assert_eq!(fits, Some(1000), "datagram {got:?}");
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
                Some(DGRAM),
                false
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
            Some(DGRAM),
            false
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
                false,
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
            false,
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
                false,
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
                false,
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
                Some(DGRAM),
                false
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
                Some(DGRAM),
                false
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
            Some(DGRAM),
            false
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
                Some(DGRAM),
                false
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
                    Some(DGRAM),
                    false
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
                false,
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
            Some(DGRAM),
            false
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
                Some(DGRAM),
                false
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
                Some(DGRAM),
                false
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
                None,
                false
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
                Some(200),
                false
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

    /// A wish counts only with the grant, on the desk and without the env pin; then it names the
    /// screen and its topology. `main` is `second` where the desktop has no main screen.
    #[test]
    fn a_wish_counts_with_the_grant_on_the_desk_without_the_pin() {
        use ScreenWish::{Main, Monitor, Only, Second};
        let own = |t| Some((DisplayTarget::Own, Some(t)));
        let granted = |w: &ScreenWish, primary| asked(Some(w), true, true, false, primary);
        assert_eq!(granted(&Only, true), own(Topology::Exclusive));
        assert_eq!(granted(&Main, true), own(Topology::Primary));
        assert_eq!(
            granted(&Main, false),
            own(Topology::Extend),
            "no main screen"
        );
        assert_eq!(granted(&Second, true), own(Topology::Extend));
        let tv = Monitor("HDMI-A-1".into());
        assert_eq!(
            granted(&tv, true),
            Some((DisplayTarget::Monitor("HDMI-A-1".into()), None))
        );
        assert_eq!(
            asked(Some(&Only), false, true, false, true),
            None,
            "no grant"
        );
        assert_eq!(asked(Some(&Only), true, false, false, true), None, "a seat");
        assert_eq!(
            asked(Some(&tv), true, true, true, true),
            None,
            "the env pin"
        );
        assert_eq!(asked(None, true, true, false, true), None, "no wish");
    }

    /// Each reason, and the precedence 1, 4, 3, 2. Off the desk only a wish is answered, with
    /// no role; with no wish only an earlier session changes what the session asked.
    #[test]
    fn the_role_echo_names_why_a_session_did_not_get_what_it_asked() {
        use RoleReason::{AsAsked, MonitorsHeld, NoMonitors, NotAllowed, Unavailable};
        let echo = |role, reason| Some(RoleEcho { role, reason });
        let second = Some(Role::Second);
        assert_eq!(
            role_echo(true, false, false, second, true, true),
            echo(None, NotAllowed)
        );
        assert_eq!(
            role_echo(true, true, false, second, true, true),
            echo(None, NoMonitors)
        );
        assert_eq!(
            role_echo(true, true, true, second, true, true),
            echo(second, Unavailable)
        );
        assert_eq!(
            role_echo(true, true, true, second, false, true),
            echo(second, MonitorsHeld)
        );
        assert_eq!(
            role_echo(false, false, true, second, false, true),
            echo(second, MonitorsHeld),
            "the first-session rule without a wish"
        );
        assert_eq!(
            role_echo(false, false, true, second, false, false),
            echo(second, AsAsked)
        );
        assert_eq!(
            role_echo(true, true, true, None, false, false),
            echo(None, AsAsked),
            "a copy of a monitor"
        );
        assert_eq!(role_echo(false, true, false, second, false, false), None);
    }
}
