//! Session controller: connect, then pump video + stats on a worker thread while a
//! dedicated audio thread pulls and decodes the audio plane. Audio never waits behind
//! a video decode. Both feed the UI / PipeWire over channels.
//!
//! The UI keeps the `Arc<NativeClient>` from `Connected` and sends input on it
//! directly — `NativeClient` is `Sync`. Planes stay one-consumer-per-thread: video
//! here, audio on its own thread, rumble+hidout on the gamepad thread.
//!
//! Pin: [`start`] / [`SessionHandle`]. Evidence: this file; decode ladder in `video`;
//! audio plane in `audio` / `design/hi-res-audio.md`.

use crate::audio;
use crate::video::{DecodedFrame, DecodedImage, Decoder};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use punktfunk_core::reanchor::{index_gap, GateVerdict, ReanchorGate};
use punktfunk_core::PunktfunkError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `Clone` exists so a [`SessionEvent::CodecFallback`] retry can re-dial with one field
/// changed while sharing the presenter-owned `force_software` and `latch_grid` Arcs.
#[derive(Clone)]
pub struct SessionParams {
    pub host: String,
    pub port: u16,
    pub mode: Mode,
    pub compositor: CompositorPref,
    pub gamepad: GamepadPref,
    pub bitrate_kbps: u32,
    /// Requested count (2/6/8); the host echoes the resolved value.
    pub audio_channels: u8,
    /// Requested [`AUDIO_FORMATS`] spelling (`"opus"` ordinarily). A request, never a
    /// fact: this box filters it, then the host may still answer Opus — read
    /// `NativeClient::audio_codec` / `_sample_rate_hz` / `_bits` for what landed.
    /// `String` so an unknown settings-file value resolves to Opus rather than refusing
    /// a connect. `PUNKTFUNK_AUDIO_HIRES` overrides it (`requested_audio_format`).
    pub audio_format: String,
    /// Soft `quic::CODEC_*` bit (`0` = auto). The host honors it when it can; the
    /// resolved codec drives the decoder.
    pub preferred_codec: u8,
    /// `quic::CODEC_*` bits to drop from advertised decode caps. `0` ordinarily; a
    /// [`SessionEvent::CodecFallback`] retry sets it. Codec is fixed at Welcome, so a
    /// fresh Hello is the only lever.
    pub exclude_codecs: u8,
    /// Advertised `quic::VIDEO_CAP_*` bits. Default is 10-bit + HDR; `0` when the user
    /// turned HDR off. The host still gates the upgrade behind `PUNKTFUNK_10BIT`.
    pub video_caps: u8,
    /// The Full chroma switch as set. `video_caps` holds 4:4:4 only when HEVC 4:4:4
    /// decodes here; a PyroWave session asks on this alone, as it decodes 4:4:4 on any GPU.
    pub want_444: bool,
    /// This panel's HDR volume, riding `Hello::display_hdr` into the host EDID so apps
    /// tone-map here. `None` = unknown/SDR. `PUNKTFUNK_CLIENT_PEAK_NITS` synthesizes a
    /// BT.2020 volume at that peak.
    pub display_hdr: Option<punktfunk_core::quic::HdrMeta>,
    pub mic_enabled: bool,
    /// Echo cancellation on the uplink. Ignored when `mic_enabled` is false;
    /// `PUNKTFUNK_NO_AEC=1` forces it off.
    pub echo_cancel: bool,
    /// DualSense voice-coil haptics (0xD1 kind 0). With `pad_speaker` it gates
    /// `CLIENT_CAP_PAD_AUDIO` and the pad-audio renderer thread.
    pub pad_haptics: bool,
    /// DualSense speaker stream (0xD1 kind 1): `"pad"` | `"mix"` | `"off"`. `"mix"`
    /// currently renders as off — see [`crate::pad_audio::speaker_active`].
    pub pad_speaker: String,
    /// Per-host clipboard share. The bridge also needs `HOST_CAP_CLIPBOARD`.
    pub clipboard: bool,
    /// Ask the host to keep its default playback device (not a silent endpoint).
    /// Request-only — an older host ignores it.
    pub keep_host_audio: bool,
    /// How the presenter fills the window; rides the Hello so a host framing the picture for
    /// another device reframes it for this one.
    pub video_fit: punktfunk_core::video_fit::VideoFit,
    /// Advertise `CLIENT_CAP_CURSOR`: this embedder draws the host cursor locally, so
    /// the host may stop compositing it. Only set when it actually draws — advertising
    /// without rendering streams with no visible cursor.
    pub cursor_forward: bool,
    /// Decoder preference; `PUNKTFUNK_DECODER` overrides — see `video::Decoder::new`.
    pub decoder: String,
    /// Library id for the host to launch (`"steam:570"`); `None` = desktop session.
    pub launch: Option<String>,
    /// Presenter's shared Vulkan device, when it can run Vulkan Video (decode lands as
    /// VkImages the presenter samples).
    pub vulkan: Option<crate::video::VulkanDecodeDevice>,
    /// Pinned host fingerprint; `None` = trust on first use (caller persists the observed one).
    pub pin: Option<[u8; 32]>,
    pub identity: (String, String),
    /// Handshake budget. The "request access" path must exceed the host's approval
    /// window — the host parks until Approve (`PENDING_APPROVAL_WAIT`).
    pub connect_timeout: Duration,
    /// Presenter raises this when hardware frames cannot be displayed. The pump demotes
    /// to software and re-requests a keyframe. Decode still succeeds in that state, so
    /// without this the stream stays black.
    pub force_software: Arc<AtomicBool>,
    /// Settings preset these params were resolved with (`None` = globals). Display
    /// only — values are already baked in; it rides so the overlay can name the preset
    /// without re-reading a store.
    pub preset: Option<String>,
    /// That preset's stable id: the dial names it to the host, which shows it and hands it to
    /// hooks and plugins. `None` with `preset` from an older spec just names nothing.
    pub preset_id: Option<String>,
    /// Overlay tier this launch resolved to. Presentation-only: the controller never
    /// reads it. It rides so a browse-mode presenter (one window, many sessions) can
    /// adopt a per-launch choice; the in-stream cycle chord still wins for that stream.
    pub stats_verbosity: crate::trust::StatsVerbosity,
    /// Overlay vocabulary this launch resolved: Standard (`false`) or Advanced. Rides per
    /// launch like the tier, so a browse-mode presenter adopts a change made between streams.
    pub advanced_stats: bool,
    /// Advertise `CLIENT_CAP_PHASE_LOCK`: the presenter has real on-glass latch stamps
    /// (`VK_KHR_present_wait`) and will feed [`latch_grid`](Self::latch_grid). Never
    /// set without present timing — the host arms on report receipt.
    pub phase_lock: bool,
    pub latch_grid: Arc<LatchGrid>,
}

/// Presenter → pump latch grid (the `force_software` pattern the other way). The
/// presenter's 1 Hz fold writes an on-glass latch plus panel period; the pump folds
/// AU arrivals against them into the ~1 Hz `PhaseReport`. All zeros until the first
/// fold — and forever without present timing — so the pump stays quiet then.
#[derive(Default)]
pub struct LatchGrid {
    /// Recent on-glass latch (client `CLOCK_REALTIME` ns — same domain as AU arrivals).
    /// Any grid point works; the report extrapolates forward.
    pub anchor_ns: std::sync::atomic::AtomicU64,
    /// Panel latch period (ns). `0` = no grid yet.
    pub period_ns: std::sync::atomic::AtomicU64,
}

/// Host, pin, launch, and budget for one dial.
///
/// The pin is the parsed form of the plan's `host.fp_hex`. Device handles stay
/// on [`Probes`]: a plan is serialised across the shell.
pub struct Dial {
    pub host: String,
    pub port: u16,
    pub pin: [u8; 32],
    pub launch: Option<String>,
    pub connect_timeout: Duration,
}

/// Device facts the plan does not own. The gamepad value is the Hello pref
/// the caller already resolved; opening the pad service stays in the session binary.
pub struct Probes {
    pub mode: Mode,
    pub vulkan: Option<crate::video::VulkanDecodeDevice>,
    pub display_hdr: Option<punktfunk_core::quic::HdrMeta>,
    /// This display can present HDR. Caps and the panel volume also require the
    /// HDR setting. A panel volume may still be absent.
    pub hdr_enabled: bool,
    pub identity: (String, String),
    pub gamepad: GamepadPref,
    pub force_software: Arc<AtomicBool>,
    pub latch_grid: Arc<LatchGrid>,
    pub stats_verbosity: crate::trust::StatsVerbosity,
    /// This device decodes HEVC 4:4:4. Caps AND this with the Full chroma switch.
    pub hevc_444_hardware: bool,
}

impl SessionParams {
    /// One fill for a resolved spec and a [`Dial`]. Probes stay off the plan.
    ///
    /// A zero width, height, or refresh inherits `mode`. Refresh 0 becomes
    /// `mode.refresh_hz.max(30)`. `exclude_codecs` stays 0. `want_444` is the
    /// switch; caps carry 4:4:4 only when `hevc_444_hardware` is set too.
    /// HDR caps need the setting and [`Probes::hdr_enabled`]. The panel volume
    /// follows the probe alone. The caller builds [`Probes::latch_grid`].
    pub fn from_plan(
        settings: &crate::trust::Settings,
        clipboard: bool,
        preset: Option<String>,
        preset_id: Option<String>,
        dial: Dial,
        probes: Probes,
    ) -> Self {
        let mode = Mode {
            width: if settings.width == 0 {
                probes.mode.width
            } else {
                settings.width
            },
            height: if settings.height == 0 {
                probes.mode.height
            } else {
                settings.height
            },
            refresh_hz: if settings.refresh_hz == 0 {
                probes.mode.refresh_hz.max(30)
            } else {
                settings.refresh_hz
            },
        };
        let (width, height) = punktfunk_core::render_scale::apply(
            mode.width,
            mode.height,
            settings.render_scale,
            punktfunk_core::render_scale::max_dimension(&settings.codec),
        );
        let mode = Mode {
            width,
            height,
            ..mode
        };
        let phase_lock = probes.vulkan.as_ref().is_some_and(|v| v.present_timing);
        let caps_444 = settings.enable_444 && probes.hevc_444_hardware;
        let advertise_hdr = settings.hdr_enabled && probes.hdr_enabled;
        // The host writes the volume into its display's EDID, so it rides only with HDR on.
        let display_hdr = advertise_hdr.then_some(probes.display_hdr).flatten();
        Self {
            host: dial.host,
            port: dial.port,
            mode,
            compositor: CompositorPref::from_name(&settings.compositor)
                .unwrap_or(CompositorPref::Auto),
            gamepad: probes.gamepad,
            bitrate_kbps: settings.bitrate_kbps,
            audio_channels: settings.audio_channels,
            audio_format: settings.audio_format.clone(),
            preferred_codec: settings.preferred_codec(),
            exclude_codecs: 0,
            video_caps: crate::video::video_caps_for(advertise_hdr, settings.ten_bit_sdr, caps_444),
            want_444: settings.enable_444,
            display_hdr,
            mic_enabled: settings.mic_enabled,
            echo_cancel: settings.echo_cancel,
            pad_haptics: settings.pad_haptics,
            pad_speaker: settings.pad_speaker.clone(),
            clipboard,
            keep_host_audio: settings.keep_host_audio,
            video_fit: punktfunk_core::video_fit::VideoFit::from_name(&settings.video_fit),
            cursor_forward: settings.mouse_mode() == crate::trust::MouseMode::Desktop,
            decoder: settings.decoder.clone(),
            launch: dial.launch,
            vulkan: probes.vulkan,
            pin: Some(dial.pin),
            identity: probes.identity,
            connect_timeout: dial.connect_timeout,
            force_software: probes.force_software,
            preset,
            preset_id,
            stats_verbosity: probes.stats_verbosity,
            advanced_stats: settings.advanced_stats,
            phase_lock,
            latch_grid: probes.latch_grid,
        }
    }
}

/// Decode-side facts the overlay window cannot read off the connector, about once a
/// second. Levels, not a window: the presenter diffs `health` over its own window.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecodeFacts {
    /// Path frames actually took (the `stats:` tag spelling). Empty until the first frame.
    pub decoder: &'static str,
    /// Session-cumulative integrity counters. `None` on a lane that cannot see damage.
    pub health: Option<crate::video::DecodeHealth>,
}

pub enum SessionEvent {
    Connected {
        connector: Arc<NativeClient>,
        mode: Mode,
        fingerprint: [u8; 32],
    },
    /// `trust_rejected` is set on a TLS trust failure (`Crypto`): for a pinned connect
    /// this is the fingerprint-changed signal, so the UI can offer re-pair rather than
    /// a dead-end error.
    Failed {
        msg: String,
        trust_rejected: bool,
    },
    Ended(Option<String>),
    /// Negotiated codec ran out of decode rungs; the client can finish only as a
    /// different codec. Terminal like [`Self::Ended`], with the retry already computed.
    /// An embedder that does not retry must still show `msg` and stop. A separate
    /// variant so the compiler asks every embedder once, rather than a flag on `Ended`.
    CodecFallback {
        /// [`SessionParams::exclude_codecs`] for the retry — derived from `retry_caps`, so
        /// applying it advertises exactly those caps.
        exclude_codecs: u8,
        /// Caps the retry will advertise — non-empty by construction.
        retry_caps: u8,
        msg: String,
    },
    /// About once a second while the pump runs.
    DecodeFacts(DecodeFacts),
    /// Session access, once after [`Self::Connected`] from Welcome, then on every
    /// mid-session `AccessUpdate`. Latest wins. `notice` is the toast line for a change
    /// worth interrupting for; `None` on the initial snapshot. The host enforces the
    /// mask regardless. A default (full, permanent — every old host) must look as today.
    Access {
        access: crate::access::SessionAccess,
        notice: Option<String>,
    },
    /// A toast line the stream needs the user to read: what did not happen, then the
    /// next move. Once per condition per session; the stream keeps running.
    Notice(String),
}

/// Times this process has had a session codec exhaust the decode ladder.
/// Process-scoped: a box whose hardware decode is broken produces one per connect;
/// the rate across sessions is the signal. Read with [`codec_fallbacks`].
static CODEC_FALLBACKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// See [`CODEC_FALLBACKS`]. Surfaced on Detailed stats as `codec_fallbacks <N>`
/// once nonzero — last, never removed, never reordered.
pub fn codec_fallbacks() -> u64 {
    CODEC_FALLBACKS.load(Ordering::Relaxed)
}

/// In-stream microphone mute, shared by the embedder's toggle and the capture
/// callback. Two flags: `live` is raised only once the uplink is running, so the
/// indicator tracks a real mute surface. Per session and never persisted — a mute
/// is a moment, not a preference; every new session starts unmuted.
#[derive(Clone, Default)]
pub struct MicControl {
    muted: Arc<AtomicBool>,
    live: Arc<AtomicBool>,
}

impl MicControl {
    pub fn live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    pub fn muted(&self) -> bool {
        self.live() && self.muted.load(Ordering::Relaxed)
    }

    /// Flip mute. `None` when this session has no uplink — the caller says so rather
    /// than pretending something happened.
    pub fn toggle(&self) -> Option<bool> {
        if !self.live() {
            return None;
        }
        let next = !self.muted.load(Ordering::Relaxed);
        self.muted.store(next, Ordering::Relaxed);
        Some(next)
    }

    fn flag(&self) -> Arc<AtomicBool> {
        self.muted.clone()
    }

    fn set_live(&self, live: bool) {
        self.live.store(live, Ordering::Relaxed);
    }
}

pub struct SessionHandle {
    pub events: async_channel::Receiver<SessionEvent>,
    pub frames: async_channel::Receiver<DecodedFrame>,
    pub stop: Arc<AtomicBool>,
    /// In-stream mic mute. Inert (`live()` false) until the uplink is running, and for
    /// the whole session when the mic is off in Settings.
    pub mic: MicControl,
    /// Pump thread. A Vulkan-Video pump submits to the shared device's decode queue —
    /// join this before any `vkDeviceWaitIdle` / teardown (external-sync over every
    /// device queue).
    pub thread: Option<std::thread::JoinHandle<()>>,
}

pub fn start(params: SessionParams) -> SessionHandle {
    let (ev_tx, ev_rx) = async_channel::unbounded();
    // Tiny frame queue, newest wins: force_send displaces the oldest when the UI lags.
    let (frame_tx, frame_rx) = async_channel::bounded(2);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_w = stop.clone();
    let mic = MicControl::default();
    let mic_w = mic.clone();
    let thread = std::thread::Builder::new()
        .name("punktfunk-session".into())
        .spawn(move || pump(params, ev_tx, frame_tx, stop_w, mic_w))
        .expect("spawn session thread");
    SessionHandle {
        events: ev_rx,
        frames: frame_rx,
        stop,
        mic,
        thread: Some(thread),
    }
}

pub fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Session audio decoder: `0xC9` Opus or `0xD3` PCM, behind one pair of methods so
/// the pull loop is plane-agnostic. The plane is chosen once from
/// `Welcome::audio_codec` and never changes (output device is open at a fixed
/// format). Both planes share a header, so this type is the only thing that knows.
/// Both arms return interleaved sample counts so the loop sizes pushes, concealment
/// and ring reporting from one number.
struct AudioDec {
    /// Host-resolved channel count, to turn libopus per-channel counts into interleaved.
    channels: usize,
    kind: DecKind,
}

enum DecKind {
    Stereo(opus::Decoder),
    Surround(opus::MSDecoder),
    /// Lossless plane: no codec state, only the negotiated depth. A lossless format has
    /// no PLC; `PcmConceal` repeats and fades instead.
    Pcm {
        bits: u8,
        conceal: punktfunk_core::audio::pcm::PcmConceal,
    },
}

impl AudioDec {
    /// Build for the plane the host resolved — `codec`/`rate_hz`/`bits` off Welcome,
    /// never off what this client asked for.
    fn new(
        codec: u8,
        channels: u8,
        rate_hz: u32,
        bits: u8,
        layout: punktfunk_core::audio::AudioLayout,
    ) -> Result<AudioDec, opus::Error> {
        let ch = channels.max(1) as usize;
        // A lossless session never reaches libopus. libopus accepts only
        // 8/12/16/24/48 kHz, which is why the hi-res ladder is a second plane.
        if codec == punktfunk_core::quic::AUDIO_CODEC_PCM {
            // Depth is the unpack stride: core reads anything that is not 16 as 24, and
            // a mismatch desyncs every sample after the first. Warn rather than refuse
            // (silence); negotiation should never produce this.
            if !punktfunk_core::audio::pcm::depth_is_supported(bits) {
                tracing::warn!(
                    bits,
                    "the host resolved a lossless depth this plane does not define — unpacking \
                     as 24-bit, which will be wrong if it meant anything else"
                );
            }
            return Ok(AudioDec {
                channels: ch,
                kind: DecKind::Pcm {
                    bits,
                    conceal: punktfunk_core::audio::pcm::PcmConceal::new(),
                },
            });
        }
        // Opus is 48 kHz by construction. Taking the rate from Welcome (not a second
        // literal) keeps the decoder, ring, and A/V-sync loop on the same millisecond.
        let kind = if channels == 2 {
            DecKind::Stereo(opus::Decoder::new(rate_hz, opus::Channels::Stereo)?)
        } else {
            let l = punktfunk_core::audio::layout_for(channels, layout);
            DecKind::Surround(opus::MSDecoder::new(
                rate_hz, l.streams, l.coupled, l.mapping,
            )?)
        };
        Ok(AudioDec { channels: ch, kind })
    }

    /// Decode one arrived frame into `out`; returns interleaved sample count.
    /// `out` is caller scratch. Opus decodes into a fixed slice, so it must already
    /// hold the biggest frame the plane can carry. PCM hands the Vec to `pcm::to_f32`,
    /// which grows it — a malformed oversized datagram cannot overrun there.
    fn decode(&mut self, input: &[u8], out: &mut Vec<f32>) -> Option<usize> {
        let channels = self.channels;
        match &mut self.kind {
            DecKind::Stereo(d) => d.decode_float(input, out, false).ok().map(|n| n * channels),
            DecKind::Surround(d) => d.decode_float(input, out, false).ok().map(|n| n * channels),
            DecKind::Pcm { bits, conceal } => {
                // `None` is a truncated datagram — a partial sample would desync every
                // sample after it. The caller treats it as a lost frame.
                let n = punktfunk_core::audio::pcm::to_f32(input, *bits, out)?;
                conceal.accept(&out[..n]);
                Some(n)
            }
        }
    }

    /// Synthesise one missing-datagram frame into `out`. `None` = nothing decoded yet;
    /// the caller should let the ring re-prime. `interleaved` is the last good frame's
    /// length — libopus PLC synthesises exactly the slice it is handed; PCM ignores it.
    fn conceal(&mut self, interleaved: usize, out: &mut Vec<f32>) -> Option<usize> {
        let channels = self.channels;
        match &mut self.kind {
            // Length read here is this frame's, not the previous call's — `PcmConceal`
            // already holds the frame it repeats, and reports `false` before anything arrives.
            DecKind::Pcm { conceal, .. } => conceal.conceal(out).then_some(out.len()),
            libopus => {
                // libopus PLC synthesises exactly the slice it is handed; before anything
                // has decoded there is no frame length to ask it for.
                let plc = interleaved.min(out.len());
                if plc == 0 {
                    return None;
                }
                let per_ch = match libopus {
                    DecKind::Stereo(d) => d.decode_float(&[], &mut out[..plc], false).ok()?,
                    DecKind::Surround(d) => d.decode_float(&[], &mut out[..plc], false).ok()?,
                    DecKind::Pcm { .. } => unreachable!("the PCM arm matched above"),
                };
                Some(per_ch * channels)
            }
        }
    }
}

// Audio-format vocabulary lives in `audio_format` so the Skia console can read it on
// Android, where nothing else in this file compiles. Re-exported so desktop callers'
// `session::AUDIO_FORMATS` spelling stays valid.
pub use crate::audio_format::{
    audio_format_wire, AUDIO_FORMATS, AUDIO_FORMAT_LOSSLESS_48, AUDIO_FORMAT_LOSSLESS_96,
    AUDIO_FORMAT_OPUS,
};

/// Lossless format this client asks the host for — `Some((rate_hz, bits))` when on,
/// `None` for Opus. The environment wins: `PUNKTFUNK_AUDIO_HIRES` overrides `setting`.
///
/// Grammar: `1`/`true`/`on`/`yes` → 96 kHz/24-bit; `96000` → that rate at 24-bit;
/// `<48000|96000>/<16|24>` → an explicit pair (`48000/16` is the cheapest lossless
/// rung — see [`AUDIO_FORMAT_UNSPECIFIED`]). `0`/`off`/`false`/`no` force Opus even
/// when the setting asks for lossless.
///
/// Unset vs unparseable are not "off". Unset: the setting decides. A typo is warned
/// and ignored, so the setting still decides — treating garbage as off would silently
/// defeat a UI switch.
fn requested_audio_format(setting: &str) -> Option<(u32, u8)> {
    resolve_audio_format(
        std::env::var("PUNKTFUNK_AUDIO_HIRES").ok().as_deref(),
        setting,
    )
}

/// Precedence half of [`requested_audio_format`], split so the env-beats-setting
/// rule is testable without mutating the process environment.
fn resolve_audio_format(env: Option<&str>, setting: &str) -> Option<(u32, u8)> {
    let Some(raw) = env else {
        return audio_format_wire(setting);
    };
    match parse_audio_format(raw) {
        AudioRequest::Legacy => None,
        AudioRequest::Hires(rate, bits) => Some((rate, bits)),
        AudioRequest::Unsupported => {
            // The user set a lever and is not getting it. Fallback is the Settings
            // choice — name it rather than promising Opus.
            tracing::warn!(
                value = %raw,
                setting,
                "PUNKTFUNK_AUDIO_HIRES is not a format this client can ask for — use 1, \
                 96000, 0, or <48000|96000>/<16|24>; ignoring it and using the audio-format \
                 setting instead"
            );
            audio_format_wire(setting)
        }
    }
}

/// Hello's "I did not ask" pair — keeps Hello byte-identical to pre-hi-res builds.
/// Not an explicit 48 000/16. Core keys `CLIENT_CAP_AUDIO_HIRES` on *a format was
/// specified*, and 48 kHz/16-bit is both the default and the cheapest lossless rung,
/// so a "differs from default" rule would make it unaskable. Passing 48 000/16 here
/// would advertise hi-res on every ordinary session.
const AUDIO_FORMAT_UNSPECIFIED: (u32, u8) = (0, 0);

#[derive(Debug, PartialEq, Eq)]
enum AudioRequest {
    /// Unset or deliberately off — Opus plane, no capability bit.
    Legacy,
    Hires(u32, u8),
    Unsupported,
}

/// Parse half of [`requested_audio_format`], testable without touching the process
/// environment.
fn parse_audio_format(raw: &str) -> AudioRequest {
    use punktfunk_core::audio::pcm::{BITS_16, BITS_24};
    let v = raw.trim().to_ascii_lowercase();
    match v.as_str() {
        "" | "0" | "off" | "false" | "no" => return AudioRequest::Legacy,
        // 24-bit is the rung the plane earns its bandwidth at: 16-bit PCM spends
        // 1.5 Mbps to sound like transparent 256 kbps Opus, so a bare "on" is not that.
        "1" | "on" | "true" | "yes" => return AudioRequest::Hires(96_000, BITS_24),
        _ => {}
    }
    // Both halves are checked against the plane. 44.1 kHz and its multiples are
    // absent on purpose: they truncate `JitterPolicy`'s integer samples-per-ms.
    // 48 000/16 is accepted — cheapest lossless rung — which is why the caller
    // sends unspecified `0`/`0` when nobody asked, not an explicit 48 000/16.
    let (rate_s, bits_s) = v.split_once('/').unwrap_or((v.as_str(), "24"));
    match rate_s
        .trim()
        .parse::<u32>()
        .ok()
        .zip(bits_s.trim().parse::<u8>().ok())
        .filter(|&(r, b)| matches!(r, 48_000 | 96_000) && matches!(b, BITS_16 | BITS_24))
    {
        Some((r, b)) => AudioRequest::Hires(r, b),
        None => AudioRequest::Unsupported,
    }
}

struct ConnectPlan {
    preferred: u8,
    pad_speaker_on: bool,
    pad_audio_on: bool,
    advertised_codecs: u8,
    bitrate_kbps: u32,
    video_caps: u8,
}

fn connect_plan(params: &SessionParams) -> ConnectPlan {
    // `PUNKTFUNK_PREFER_PYROWAVE=1`: the host only ever picks PyroWave when the
    // client names it as `preferred_codec`.
    #[allow(unused_mut)]
    let mut preferred = params.preferred_codec;
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    if std::env::var("PUNKTFUNK_PREFER_PYROWAVE").as_deref() == Ok("1") {
        if params.vulkan.as_ref().is_some_and(|v| v.pyrowave_decode) {
            preferred = punktfunk_core::quic::CODEC_PYROWAVE;
        } else {
            tracing::warn!(
                "PUNKTFUNK_PREFER_PYROWAVE=1 but the presenter device failed the pyrowave probe — keeping the normal codec preference"
            );
        }
    }
    // Advertise pad audio only when settings could render a stream. Per-pad
    // detection at slot open still decides which pads declare render caps.
    let pad_speaker_on = crate::pad_audio::speaker_active(&params.pad_speaker);
    let pad_audio_on = params.pad_haptics || pad_speaker_on;
    // What this session advertises it can decode, minus anything a previous attempt
    // proved it cannot finish. Held for the whole pump: the reconnect rule needs
    // what was on the table, not just what the host picked.
    let advertised_codecs = crate::video::decodable_codecs_for(
        params.vulkan.as_ref(),
        // A session pinned to software has no HEVC rung; advertising HEVC would
        // promise what this build cannot keep.
        &params.decoder,
    ) & !params.exclude_codecs;
    // Gated on the codec actually being advertised, so a failed probe or a retry that
    // dropped it falls back to H.26x with the user's rate and the HEVC-gated caps.
    let pyrowave = preferred == punktfunk_core::quic::CODEC_PYROWAVE
        && advertised_codecs & punktfunk_core::quic::CODEC_PYROWAVE != 0;
    // PyroWave is always Automatic bitrate: a fixed kbps is ill-defined for the
    // all-intra codec (bpp is the operating point) and used to bypass the host
    // ceiling. Send 0; the stored preset value is untouched.
    let bitrate_kbps = if pyrowave {
        if params.bitrate_kbps != 0 {
            tracing::info!(
                stored_kbps = params.bitrate_kbps,
                "PyroWave forces Automatic bitrate — asking the host for its per-mode pin"
            );
        }
        0
    } else {
        params.bitrate_kbps
    };
    // PyroWave decodes 4:4:4 on any GPU, so it asks on the switch alone. The host pins
    // its rate from the chroma it grants.
    let video_caps = if pyrowave && params.want_444 {
        params.video_caps | punktfunk_core::quic::VIDEO_CAP_444
    } else {
        params.video_caps
    };
    if params.exclude_codecs != 0 {
        tracing::info!(
            excluded = params.exclude_codecs,
            advertising = advertised_codecs,
            "retrying with reduced decode caps"
        );
    }
    ConnectPlan {
        preferred,
        pad_speaker_on,
        pad_audio_on,
        advertised_codecs,
        bitrate_kbps,
        video_caps,
    }
}

fn open_decoder(
    decoder: &str,
    vulkan: Option<&crate::video::VulkanDecodeDevice>,
    connector: &NativeClient,
) -> anyhow::Result<Decoder> {
    // Decoder for the host-resolved codec and picture shape (never assume HEVC).
    // Native rungs probe the device at construction, so a 4:4:4 or Main 10 this GPU
    // cannot decode refuses before the rung is chosen instead of error-streaking.
    let stream_format = crate::video::StreamFormat {
        chroma_format_idc: connector.chroma_format,
        bit_depth: connector.bit_depth,
    };
    tracing::info!(
        codec = crate::video::wire_codec_name(connector.codec),
        welcome_codec = connector.codec,
        "negotiated video codec"
    );
    // A negotiated PyroWave session decodes on the presenter's device — reachable
    // only through the explicit preference above, so failing here is failing an
    // opted-in experiment.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    let built = if connector.codec == punktfunk_core::quic::CODEC_PYROWAVE {
        let mode = connector.mode();
        // Wavelet bitstream has no VUI: Welcome colour signalling is the session's
        // colour contract, and the resolved chroma sizes the plane ring.
        let color = crate::video::ColorDesc {
            primaries: connector.color.primaries,
            transfer: connector.color.transfer,
            matrix: connector.color.matrix,
            full_range: connector.color.full_range != 0,
        };
        match vulkan {
            Some(vk) => Decoder::new_pyrowave(
                vk,
                mode.width,
                mode.height,
                connector.shard_payload as usize,
                connector.chroma_format == punktfunk_core::quic::CHROMA_IDC_444,
                color,
                connector.bit_depth >= 10,
            ),
            None => Err(anyhow::anyhow!(
                "pyrowave session without a presenter device"
            )),
        }
    } else {
        Decoder::new(connector.codec, decoder, vulkan, stream_format)
    };
    #[cfg(not(all(any(target_os = "linux", windows), feature = "pyrowave")))]
    let built = Decoder::new(connector.codec, decoder, vulkan, stream_format);
    built
}

/// [`crate::video::amd_vulkan_hdr_driver_notice`] for this session, AMD only. The driver
/// version is logged either way: without it a triage log cannot tell old drivers apart.
#[cfg(windows)]
fn amd_vulkan_hdr_notice(
    decoder: &Decoder,
    vulkan: Option<&crate::video::VulkanDecodeDevice>,
    connector: &NativeClient,
) -> Option<String> {
    const VENDOR_AMD: u32 = 0x1002;
    let vk = vulkan.filter(|v| v.vendor_id == VENDOR_AMD)?;
    let driver = crate::video_d3d11::adapter_driver_version(vk.adapter_luid?);
    tracing::info!(?driver, "AMD display driver");
    crate::video::amd_vulkan_hdr_driver_notice(
        driver,
        decoder.on_native_vulkan(),
        connector.color.transfer == punktfunk_core::quic::ColorInfo::TRC_PQ,
    )
}

struct PlaneThreads {
    audio_thread: Option<std::thread::JoinHandle<()>>,
    pad_audio_thread: Option<std::thread::JoinHandle<()>>,
    clipboard_thread: Option<std::thread::JoinHandle<()>>,
    mic_uplink: Option<audio::MicStreamer>,
}

struct PlaneSettings {
    pad_audio_on: bool,
    pad_speaker_on: bool,
    pad_haptics: bool,
    clipboard: bool,
    mic_enabled: bool,
    echo_cancel: bool,
}

/// Send the remembered lost range once the shared 100 ms ask throttle is open.
/// A span wider than `RFI_MAX_RANGE` is beyond any encoder's history: keyframe.
fn flush_pending_rfi(
    pending: &mut Option<(u32, u32)>,
    last_req: &mut Option<Instant>,
    now: Instant,
    connector: &NativeClient,
) {
    let throttled = last_req.is_some_and(|t| now.duration_since(t) < Duration::from_millis(100));
    let Some((first, last)) = pending.filter(|_| !throttled) else {
        return;
    };
    *pending = None;
    *last_req = Some(now);
    if last.wrapping_sub(first).wrapping_add(1) > punktfunk_core::packet::RFI_MAX_RANGE {
        let _ = connector.request_keyframe();
    } else {
        let _ = connector.request_rfi(first, last);
    }
}

fn spawn_plane_threads(
    connector: &Arc<NativeClient>,
    stop: &Arc<AtomicBool>,
    access: crate::access::SessionAccess,
    mic: &MicControl,
    settings: PlaneSettings,
) -> PlaneThreads {
    // Audio is best-effort: a session without it still streams. Gamepads are the
    // app-lifetime service. Audio runs on its own thread (one puller per plane).
    let audio_thread = spawn_audio(connector.clone(), stop.clone());
    // Own drain thread. The output device opens lazily once frames arrive — a
    // session without a wired DualSense costs one idle 10 ms poll loop.
    let pad_audio_thread = settings
        .pad_audio_on
        .then(|| {
            crate::pad_audio::spawn(
                connector.clone(),
                stop.clone(),
                settings.pad_haptics,
                settings.pad_speaker_on,
            )
        })
        .flatten();
    // Own thread: `next_clip` blocks and OS clipboard calls can wait on other apps.
    // Host without clipboard capability returns immediately. Also gated by the
    // session's CLIPBOARD grant — without it the host coordinator never starts.
    let clipboard_thread = (settings.clipboard
        && access.allows(punktfunk_core::quic::GRANT_CLIPBOARD))
    .then(|| {
        let c = connector.clone();
        let s = stop.clone();
        std::thread::Builder::new()
            .name("pf-clipboard".into())
            .spawn(move || crate::clipboard::run(c, s))
            .ok()
    })
    .flatten();
    // `set_live` makes the chord real. Disabled settings, failed capture, or no
    // MIC grant leaves it false because the host would drop uplink datagrams.
    let mic_uplink = (settings.mic_enabled && access.allows(punktfunk_core::quic::GRANT_MIC))
        .then(|| {
            audio::MicStreamer::spawn(connector.clone(), mic.flag(), settings.echo_cancel)
                .map_err(|e| tracing::warn!(error = %e, "mic uplink disabled"))
                .ok()
        })
        .flatten();
    mic.set_live(mic_uplink.is_some());
    PlaneThreads {
        audio_thread,
        pad_audio_thread,
        clipboard_thread,
        mic_uplink,
    }
}

fn pump(
    params: SessionParams,
    ev_tx: async_channel::Sender<SessionEvent>,
    frame_tx: async_channel::Sender<DecodedFrame>,
    stop: Arc<AtomicBool>,
    mic: MicControl,
) {
    let ConnectPlan {
        preferred,
        pad_speaker_on,
        pad_audio_on,
        advertised_codecs,
        bitrate_kbps,
        video_caps,
    } = connect_plan(&params);
    // Lossless opt-in, filtered by what this box can play. `CLIENT_CAP_AUDIO_HIRES`
    // means capable *and* the user turned it on — advertising without being able to
    // render spends 1.5–4.6 Mbps ABR cannot reclaim. `Some` past this block is
    // "ask", and asking is what sets the bit (`AUDIO_FORMAT_UNSPECIFIED`).
    let hires = requested_audio_format(&params.audio_format).filter(|&(rate, _)| {
        if params.audio_channels != 2 {
            // A hi-res surround frame does not fit one datagram at the default MTU
            // and this plane is never fragmented. The two settings are independent
            // overlay keys, and the env override answers to no UI, so this is the
            // one place both are known.
            tracing::warn!(
                channels = params.audio_channels,
                "lossless audio is stereo-only — a surround frame does not fit one QUIC \
                 datagram; asking for the default Opus plane instead"
            );
            return false;
        }
        audio::can_render_at(rate)
    });
    if let Some((rate, bits)) = hires {
        tracing::info!(rate, bits, "asking the host for the lossless audio plane");
    }
    // This pair is the request: core derives the cap from it being specified, so
    // `None` must reach the wire as unspecified, not as an explicit 48 000/16.
    let (audio_rate_hz, audio_bits) = hires.unwrap_or(AUDIO_FORMAT_UNSPECIFIED);
    // Per dial: a session without a preset must not name the last one's.
    punktfunk_core::client::set_session_preset(params.preset_id.as_deref().and_then(|id| {
        punktfunk_core::quic::SessionPreset::new(id, params.preset.as_deref().unwrap_or(""))
    }));
    let connector = match NativeClient::connect_with_audio_format(
        &params.host,
        params.port,
        params.mode,
        params.compositor,
        params.gamepad,
        bitrate_kbps,
        video_caps,
        params.audio_channels,
        audio_rate_hz,
        audio_bits,
        // Legacy coupling: this client decodes either, and only NDL-class sinks need the other.
        punktfunk_core::audio::AudioLayout::Legacy,
        params.video_fit,
        advertised_codecs,
        preferred,
        // Env hatch wins so an A/B run can pin an exact peak (`PUNKTFUNK_CLIENT_PEAK_NITS`).
        punktfunk_core::client::display_hdr_env_override().or(params.display_hdr),
        (if params.cursor_forward {
            punktfunk_core::quic::CLIENT_CAP_CURSOR
        } else {
            0
        }) | (if params.phase_lock {
            punktfunk_core::quic::CLIENT_CAP_PHASE_LOCK
        } else {
            0
            // AUDIO_HIRES is not set here: core derives it from the format pair being
            // specified, which is the one rule that keeps 48 kHz/16-bit lossless
            // askable. Setting it without a format would advertise a request the host
            // can only decline.
        }) | (if pad_audio_on {
            punktfunk_core::quic::CLIENT_CAP_PAD_AUDIO
        } else {
            0
        }) | (if params.keep_host_audio {
            punktfunk_core::quic::CLIENT_CAP_KEEP_HOST_AUDIO
        } else {
            0
        }),
        // Slice-progressive delivery: off — every rung here is fed whole AUs.
        false,
        params.launch.clone(),
        // Host's trust-store label. Without it every no-PIN "request access" knock
        // showed as the fingerprint placeholder "device abcd1234".
        Some(crate::trust::device_name()),
        params.pin,
        Some(params.identity),
        params.connect_timeout,
        // Session stop flag, so cancel reaches a dial that has not landed. Without
        // it this parks the pump for the whole budget (185 s on a request-access
        // connect the host holds pending) and cancel cannot be answered until return.
        Some(stop.clone()),
    ) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            let trust_rejected = matches!(e, PunktfunkError::Crypto);
            let msg = match e {
                PunktfunkError::Crypto => {
                    "Host identity rejected — wrong fingerprint, or the host requires pairing"
                        .to_string()
                }
                PunktfunkError::Timeout => "Connection timed out".to_string(),
                // Host said why it turned us away — show that verbatim: "denied on the
                // host" and "timed out" call for different next steps.
                PunktfunkError::Rejected(reason) => crate::trust::connect_reject_message(reason),
                other => {
                    tracing::warn!(error = %other, "connect failed");
                    "The host didn't answer".to_string()
                }
            };
            let _ = ev_tx.send_blocking(SessionEvent::Failed {
                msg,
                trust_rejected,
            });
            return;
        }
    };
    let _ = ev_tx.send_blocking(SessionEvent::Connected {
        connector: connector.clone(),
        mode: connector.mode(),
        fingerprint: connector.host_fingerprint,
    });
    // Welcome's access advert, straight after Connected so the embedder can gate
    // capture before it engages. Old hosts decode to full-control/permanent.
    let mut access = crate::access::SessionAccess::from_connector(&connector);
    let _ = ev_tx.send_blocking(SessionEvent::Access {
        access,
        notice: None,
    });

    let mut decoder = match open_decoder(&params.decoder, params.vulkan.as_ref(), &connector) {
        Ok(d) => d,
        Err(e) => {
            // No rung for this codec at all (no hardware HEVC, or pinned software on
            // an HEVC session). Same answer as the mid-stream case below.
            let refusal = e.downcast_ref::<crate::video::NoSoftwareRung>().map(|nr| {
                codec_fallback_event(
                    connector.codec,
                    advertised_codecs,
                    nr.loss(),
                    &e.to_string(),
                )
            });
            // Audio / pad / clipboard / mic are built below, so this is vacuously
            // joined. Drop in the same order as the pump's end path so a reconnect
            // on this event finds the same world whichever refusal site produced it.
            stop.store(true, Ordering::SeqCst);
            mic.set_live(false);
            drop(connector);
            let _ = ev_tx.send_blocking(
                refusal.unwrap_or_else(|| SessionEvent::Ended(Some(format!("video decoder: {e}")))),
            );
            return;
        }
    };
    // An old AMD driver can paint Vulkan-decoded HDR green: say so once, switch nothing.
    #[cfg(windows)]
    if let Some(msg) = amd_vulkan_hdr_notice(&decoder, params.vulkan.as_ref(), &connector) {
        let _ = ev_tx.send_blocking(SessionEvent::Notice(msg));
    }
    let force_software = params.force_software.clone();
    let PlaneThreads {
        audio_thread,
        pad_audio_thread,
        clipboard_thread,
        mut mic_uplink,
    } = spawn_plane_threads(
        &connector,
        &stop,
        access,
        &mic,
        PlaneSettings {
            pad_audio_on,
            pad_speaker_on,
            pad_haptics: params.pad_haptics,
            clipboard: params.clipboard,
            mic_enabled: params.mic_enabled,
            echo_cancel: params.echo_cancel,
        },
    );

    // Live host↔client clock offset, loaded per frame so mid-stream re-syncs keep
    // capture-clock latency honest — never cached at session start.
    let clock_offset_live = connector.clock_offset_shared();
    // Every received AU's arrival stamp, folded per stats window against the latch
    // grid into the ~1 Hz PhaseReport. 256 ≈ 2 s at 120 Hz.
    let latch_grid = params.latch_grid.clone();
    let mut phase_arrivals: Vec<u64> = Vec::new();
    let mut last_applied_phase: Option<i32> = None;
    // `PUNKTFUNK_DEBUG_RECONFIGURE=WxH@HZ:SECS` — request one mid-stream mode
    // switch N seconds in, so a headless session can exercise the resize path.
    let pump_start = Instant::now();
    let mut debug_reconfig = std::env::var("PUNKTFUNK_DEBUG_RECONFIGURE")
        .ok()
        .and_then(|s| {
            let parsed = parse_debug_reconfigure(&s);
            if parsed.is_none() {
                tracing::warn!(value = %s, "PUNKTFUNK_DEBUG_RECONFIGURE not understood (want WxH@HZ:SECS) — ignored");
            }
            parsed
        });
    let mut total_frames = 0u64;
    // Newest frame index handed to the decoder — the staleness bar for late partials.
    let mut newest_decoded_idx: Option<u32> = None;
    let mut window_start = Instant::now();
    // The pin-unsustainable notice goes out once per session.
    let mut pin_noticed = false;
    // The last launch verdict turned into a notice: each verdict is said once.
    let mut launch_told: Option<punktfunk_core::quic::LaunchOutcome> = None;
    // One fence-waited decode sample per window on the async rung: a per-frame wait
    // would serialize decode to 1/latency.
    let mut fence_sampled = false;
    // Report decode stage to ABR only when armed. Constant for the session.
    let wants_decode = connector.wants_decode_latency();
    // What actually decoded the last frame — VAAPI can demote mid-session.
    let mut dec_path: &'static str = "";
    let mut last_kf_req: Option<Instant> = None;
    // Lost range the ask throttle swallowed, `(first, last)`, widened by later gaps
    // and sent by `flush_pending_rfi` once the throttle opens. Without it a second
    // gap inside the window (a lost recovery anchor) asks nothing until the 500 ms
    // backstop, and then for an IDR.
    let mut pending_rfi: Option<(u32, u32)> = None;
    // PyroWave AUs decode independently, so a late one is still worth showing.
    let all_intra = connector.codec == punktfunk_core::quic::CODEC_PYROWAVE;
    // Freeze-until-reanchor. Armed on any loss signal, withholds concealed frames
    // until a clean re-anchor. Owns the no-output streak and overdue-freeze
    // backstop. Seeded with the current drop count so the first `poll` is not a loss.
    let mut gate = ReanchorGate::new(connector.frames_dropped());
    // Frame index we expect next. A jump is the earliest loss signal — ~120 ms
    // ahead of `frames_dropped` (the reassembler only declares a straggler lost
    // once it ages out of the loss window).
    let mut next_expected_index: Option<u32> = None;
    // Fixture capture of every AU as it reaches `decode_frame` (`au_dump.rs`).
    // This is what the host sent. `PUNKTFUNK_AU_FAULT` injects one level down, so
    // a faulted run's fixture is the clean bitstream and will not replay the damage.
    let mut au_dump = crate::au_dump::AuDump::from_env(connector.codec);
    // Decode-order watermark at the latest freeze arm. A frame at or below this
    // was decoded before the loss; its recovery SEI must not lift that freeze.
    // Re-stamped when `gate.arms()` moves, not on the overdue backstop (that
    // re-asks without re-arming — discarding an in-flight heal would be wrong).
    let mut gate_arms = gate.arms();
    let mut arm_decode_order: u64 = 0;
    // Set when the ladder ran out of rungs. `Some` is the only way the pump ends
    // with a retry attached.
    let mut codec_fallback: Option<SessionEvent> = None;

    let end: Option<String> = loop {
        if stop.load(Ordering::SeqCst) {
            break None;
        }
        if let Some((mode, delay)) = debug_reconfig {
            if pump_start.elapsed() >= delay {
                tracing::info!(
                    ?mode,
                    "PUNKTFUNK_DEBUG_RECONFIGURE: requesting mid-stream mode switch"
                );
                if let Err(e) = connector.request_mode(mode) {
                    tracing::warn!(error = ?e, "debug mode switch request failed");
                }
                debug_reconfig = None;
            }
        }
        // Mid-session access updates. Drain and re-read once — latest wins; the
        // connector already folded every update. The mic uplink follows its grant live.
        {
            let mut updated = false;
            while connector.next_access_update(Duration::ZERO).is_ok() {
                updated = true;
            }
            if updated {
                let prev = access;
                access = crate::access::SessionAccess::from_connector(&connector);
                let notice = crate::access::update_notice(prev.grants, &access, Instant::now());
                let mic_on = params.mic_enabled && access.allows(punktfunk_core::quic::GRANT_MIC);
                if !mic_on && mic_uplink.is_some() {
                    tracing::info!("MIC grant removed mid-session — stopping the mic uplink");
                    mic_uplink = None;
                    mic.set_live(false);
                } else if mic_on && mic_uplink.is_none() {
                    mic_uplink = audio::MicStreamer::spawn(
                        connector.clone(),
                        mic.flag(),
                        params.echo_cancel,
                    )
                    .map_err(|e| tracing::warn!(error = %e, "mic uplink disabled"))
                    .ok();
                    mic.set_live(mic_uplink.is_some());
                }
                let _ = ev_tx.send_blocking(SessionEvent::Access { access, notice });
            }
        }
        // 20 ms wait: audio has its own thread, so this only bounds stop-flag
        // responsiveness and the per-iteration recovery check. A frame arrives every
        // ~8–16 ms at 60–120 Hz, so this rarely times out mid-stream.
        match connector.next_frame(Duration::from_millis(20)) {
            Ok(frame) => {
                // Reassembly completion, stamped by the core session as the AU crossed
                // `poll_frame`. Stamping here at the pull would fold pre-decode queue
                // wait into `host+network` (client backlog looking like network).
                // 0 = a core predating the stamp; fall back to the pull instant.
                let received_ns = if frame.received_ns > 0 {
                    frame.received_ns
                } else {
                    now_ns()
                };
                if params.phase_lock && phase_arrivals.len() < 256 {
                    phase_arrivals.push(received_ns);
                }
                // Host numbers frames consecutively, so a jump means a frame is missing
                // and this AU references a picture we never decoded. Arm the freeze at
                // the first such frame — ~120 ms before `frames_dropped` — so concealment
                // never reaches the screen.
                match next_expected_index {
                    Some(exp) if frame.frame_index == exp => {
                        next_expected_index = Some(exp.wrapping_add(1));
                    }
                    // Forward gap: hold the last good frame, but do not ask for a
                    // keyframe here. Hiding concealment is free; an IDR at 4K120 is not
                    // and can re-trigger the burst. A straggler (`index_gap` → None)
                    // leaves the expectation so the real gap still trips.
                    Some(exp) => {
                        if let Some(gap) = index_gap(exp, frame.frame_index) {
                            let now = Instant::now();
                            // Credited arm: the reassembler books these lost frames into
                            // `frames_dropped` up to ~120 ms from now; the credit keeps
                            // that climb from re-freezing a stream the RFI anchor healed.
                            gate.arm_expecting_drops(now, u64::from(gap));
                            next_expected_index = Some(frame.frame_index.wrapping_add(1));
                            // The oldest unsent loss stays `first`: the host invalidates
                            // everything since it anyway, so one ask covers a burst.
                            let first = pending_rfi.map_or(exp, |(first, _)| first);
                            pending_rfi = Some((first, frame.frame_index.wrapping_sub(1)));
                            flush_pending_rfi(&mut pending_rfi, &mut last_kf_req, now, &connector);
                            tracing::trace!(
                                gap,
                                "frame gap — RFI recovery, holding last frame until re-anchor"
                            );
                        } else if !all_intra {
                            // A whole AU behind one already decoded: decoding it now
                            // rewinds the DPB (H.264 reads it as a frame_num wrap, HEVC's
                            // RPS unmarks the newer picture) and phantoms an RFI. Skip.
                            tracing::trace!(
                                index = frame.frame_index,
                                expected = exp,
                                "skipping a straggler AU that arrived behind a decoded one"
                            );
                            continue;
                        }
                    }
                    None => next_expected_index = Some(frame.frame_index.wrapping_add(1)),
                }
                // A partial that lost the race (a newer frame already decoded) is time
                // travel — skip it. Completes keep the normal path.
                if !frame.complete
                    && newest_decoded_idx
                        .is_some_and(|n: u32| n.wrapping_sub(frame.frame_index) <= u32::MAX / 2)
                {
                    continue;
                }
                newest_decoded_idx = Some(match newest_decoded_idx {
                    Some(n) if frame.frame_index.wrapping_sub(n) > u32::MAX / 2 => n,
                    _ => frame.frame_index,
                });
                if let Some(d) = au_dump.as_mut() {
                    if !d.write(&frame.data, frame.flags, frame.complete) {
                        au_dump = None;
                    }
                }
                // Re-stamp the arm watermark before this AU decodes, so it names the
                // newest picture that existed when the freeze was armed. Arms above
                // happened this iteration; sites below run after decode.
                if gate.arms() != gate_arms {
                    gate_arms = gate.arms();
                    arm_decode_order = decoder.decode_order();
                }
                match decoder.decode_frame(&frame.data, frame.flags, frame.complete) {
                    Ok(Some(image)) => {
                        // Decoder's own re-anchor first: a recovery-point SEI is the
                        // only clean point an intra-refresh session has. Pair by decode
                        // order — a DPB flush after a failed AU hands back pictures
                        // decoded before the loss, which arrive after the arm.
                        let local = match image.decode_order() {
                            Some(order) if order <= arm_decode_order => {
                                tracing::trace!(
                                    order,
                                    arm_decode_order,
                                    "discarding the local recovery of a frame decoded before \
                                     the loss"
                                );
                                punktfunk_core::reanchor::LocalRecovery::NONE
                            }
                            _ => image.local_recovery(),
                        };
                        if gate.on_local_recovery(local) {
                            decoder.forgive_unclean();
                            tracing::debug!(
                                "re-anchored on the stream's own recovery point SEI — no IDR needed"
                            );
                        }
                        // Shared freeze gate, corroborated: a frame predicting from a picture
                        // this decoder concealed is held whatever the wire says, and refuses a
                        // host RECOVERY_ANCHOR. If that arms an unfrozen gate (a lift landed
                        // before the damaged chain drained), ask for the IDR now rather than
                        // at the 500 ms backstop.
                        let evidence = image.anchor_evidence();
                        if evidence == punktfunk_core::reanchor::AnchorEvidence::ReferencesDamaged
                            && frame.flags & punktfunk_core::packet::USER_FLAG_RECOVERY_ANCHOR != 0
                        {
                            tracing::debug!(
                                "refused a host recovery anchor: this AU predicts from a picture \
                                 this decoder had to conceal — holding for a real IDR"
                            );
                        }
                        let now = Instant::now();
                        let was_holding = gate.is_holding();
                        let present = gate.on_decoded_corroborated(
                            frame.flags,
                            image.is_keyframe(),
                            evidence,
                            now,
                        ) == GateVerdict::Present;
                        // A wave lift: the planner's damaged-chain marks are stale from here,
                        // or every later host anchor is refused until an IDR.
                        if was_holding && present && gate.lifted_by_marks() {
                            decoder.forgive_unclean();
                            tracing::debug!(
                                "re-anchored on intra refresh marks — forgetting the damaged \
                                 reference chain"
                            );
                        }
                        if !present && !was_holding {
                            tracing::debug!(
                                "damaged reference chain reached an unfrozen gate — holding, \
                                 requesting keyframe"
                            );
                            if last_kf_req
                                .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100))
                            {
                                last_kf_req = Some(now);
                                let _ = connector.request_keyframe();
                            }
                        }
                        total_frames += 1;
                        // `stats:` decode-path tag is a machine interface — additive
                        // only. Surviving tags keep their exact spelling.
                        dec_path = match &image {
                            DecodedImage::Cpu(_) => "software",
                            #[cfg(target_os = "linux")]
                            DecodedImage::NativeDmabuf(_) => "native-vaapi",
                            #[cfg(windows)]
                            DecodedImage::D3d11(_) => "native-d3d11va",
                            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
                            DecodedImage::PyroWave(_) => "pyrowave",
                            DecodedImage::NativeVk(_) => "native-vulkan",
                        };
                        if total_frames == 1 {
                            let (w, h, path) = match &image {
                                DecodedImage::Cpu(c) => (c.width, c.height, "software"),
                                #[cfg(target_os = "linux")]
                                DecodedImage::NativeDmabuf(d) => {
                                    (d.width, d.height, "native-vaapi-dmabuf")
                                }
                                #[cfg(windows)]
                                DecodedImage::D3d11(d) => (d.width, d.height, "native-d3d11va"),
                                #[cfg(all(
                                    any(target_os = "linux", windows),
                                    feature = "pyrowave"
                                ))]
                                DecodedImage::PyroWave(f) => (f.width, f.height, "pyrowave"),
                                DecodedImage::NativeVk(f) => (f.width, f.height, "native-vulkan"),
                            };
                            tracing::info!(width = w, height = h, path, "first frame decoded");
                        }
                        // Travels with the frame so the presenter can measure `display`.
                        let decoded_ns = now_ns();
                        connector.hud().note_decoded(frame.pts_ns, decoded_ns);
                        // Ship first, then the decode stat. Vulkan returns at submission;
                        // a per-frame fence wait serializes to 1/decode_latency. One
                        // honest sample per window. Polling would quantize by a whole
                        // frame interval (8.3 ms at 120 Hz vs ~0.1–2 ms decodes).
                        let hw_fence = match &image {
                            // Native rung: decode signals `semaphore_value` when pixels
                            // are ready (presenter write-back is `+ 1`). Wait measures
                            // received→decode-complete.
                            DecodedImage::NativeVk(f) => Some((f.semaphore, f.semaphore_value)),
                            _ => None,
                        };
                        if present {
                            // A displaced frame decoded and was never shown: newest wins.
                            if let Ok(Some(_)) = frame_tx.force_send(DecodedFrame {
                                pts_ns: frame.pts_ns,
                                decoded_ns,
                                image,
                            }) {
                                connector.hud().note_skipped(1, 0);
                            }
                        } else {
                            // Withhold this frame so the presenter redraws the last good
                            // picture. `hw_fence` still samples (handle stays valid).
                            tracing::trace!("holding last frame — awaiting post-loss re-anchor");
                        }
                        match hw_fence {
                            // `decoded_ns` is a submission stamp here, so GPU decode sits
                            // inside `display` and this sample re-counts it.
                            Some((sem, value)) => {
                                if !fence_sampled && decoder.wait_hw_decoded(sem, value, 50_000_000)
                                {
                                    fence_sampled = true;
                                    let us = now_ns().saturating_sub(received_ns) / 1000;
                                    connector.hud().note_decode_us(us, true);
                                }
                            }
                            None => {
                                let us = decoded_ns.saturating_sub(received_ns) / 1000;
                                connector.hud().note_decode_us(us, false);
                            }
                        }
                        // ABR: decoder-backlog every frame, using the CPU-side stamp.
                        // Exact for sync paths; received→submit for async Vulkan — the
                        // backpressure the controller needs, without the fence wait.
                        if wants_decode {
                            let us = decoded_ns.saturating_sub(received_ns) / 1000;
                            connector.report_decode_us(us.min(u32::MAX as u64) as u32);
                        }
                    }
                    // No output under one-in/one-out LOW_DELAY means wedged on missing
                    // references with no reassembler drop. The gate counts the streak
                    // and, once it trips, arms the freeze and asks for an IDR.
                    Ok(None) => {
                        let now = Instant::now();
                        if gate.on_no_output(now)
                            && last_kf_req
                                .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100))
                        {
                            last_kf_req = Some(now);
                            let _ = connector.request_keyframe();
                            tracing::debug!("requested keyframe (decoder produced no output)");
                        }
                    }
                    // Last rung gone for this codec. Feeding more AUs would freeze the
                    // screen forever. Break; the terminal event carries the retry.
                    Err(e) if e.downcast_ref::<crate::video::NoSoftwareRung>().is_some() => {
                        let loss = e
                            .downcast_ref::<crate::video::NoSoftwareRung>()
                            .expect("just matched")
                            .loss();
                        codec_fallback = Some(codec_fallback_event(
                            connector.codec,
                            advertised_codecs,
                            loss,
                            &e.to_string(),
                        ));
                        break None;
                    }
                    // Survivable (loss until the next IDR/RFI) — keep feeding.
                    Err(e) => {
                        tracing::debug!(error = %e, "decode error (recovering)");
                        let now = Instant::now();
                        if gate.on_no_output(now)
                            && last_kf_req
                                .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100))
                        {
                            last_kf_req = Some(now);
                            let _ = connector.request_keyframe();
                            tracing::debug!("requested keyframe (decode error recovery)");
                        }
                    }
                }
                // Presenter: hardware frames cannot be displayed. Demote here, on the
                // decoder's thread. Decode succeeds in that state, so error-streak
                // demotion never fires.
                if force_software.swap(false, Ordering::Relaxed) {
                    if let Err(e) = decoder.force_software() {
                        break Some(format!("software decoder rebuild: {e}"));
                    }
                }
                // Infinite GOP has no periodic keyframe, so a rebuilt/erroring decoder
                // stays gray until we ask. Arm only when not already holding: this flag
                // fires per damaged AU, and every `arm` zeroes recovery-mark counts.
                // A genuine new loss still re-arms via a frame-index gap or drop climb.
                if decoder.take_keyframe_request() {
                    let now = Instant::now();
                    if !gate.is_holding() {
                        gate.arm(now);
                    }
                    if last_kf_req
                        .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100))
                    {
                        last_kf_req = Some(now);
                        let _ = connector.request_keyframe();
                        tracing::debug!("requested keyframe (decoder recovery)");
                    }
                }
            }
            Err(PunktfunkError::NoFrame) => {}
            // `None` means normal finish to every embedder. Only an ending that went
            // wrong should carry a message.
            Err(PunktfunkError::Closed) => {
                use punktfunk_core::client::PunktfunkEndReason as End;
                // A typed mid-session rejection names itself — access expiry would
                // otherwise file under HostError as "the host ended with an error".
                if let Some(reason) = connector.end_reject() {
                    break Some(crate::trust::reject_message(
                        reason,
                        connector.end_reject_said(),
                    ));
                }
                break match connector.end_reason() {
                    End::GameExited => None,
                    End::Local | End::HostEnded => None,
                    End::HostError => Some("The host ended the session with an error".to_string()),
                    End::Lost => Some("Connection lost".to_string()),
                    // No verdict (older core, or the close raced the read): keep this
                    // arm's historic wording rather than inventing a new one.
                    End::None => Some("Host ended the session".to_string()),
                };
            }
            Err(e) => {
                tracing::warn!(error = %e, "session pump failed");
                break Some("The stream stopped unexpectedly".to_string());
            }
        }

        // Drain per-AU 0xCF timings; the connector matches each to its frame for the overlay.
        while let Ok(t) = connector.next_host_timing(Duration::ZERO) {
            // Host's applied grid offset rides the 0xCF tail. Log transitions so an
            // on-glass run can watch the controller engage.
            if params.phase_lock
                && t.applied_phase_ns.is_some()
                && t.applied_phase_ns != last_applied_phase
            {
                last_applied_phase = t.applied_phase_ns;
                tracing::info!(
                    applied_phase_ns = t.applied_phase_ns.unwrap_or(0),
                    "host phase-lock: applied capture-grid offset"
                );
            }
        }

        // Loss recovery + overdue backstop through the shared gate. A drop-count
        // climb arms the freeze (decoder conceals and returns Ok). Overdue freeze
        // re-asks while holding: never resume to gray. 100 ms throttle; infinite GOP
        // means the only recovery keyframe is one we request.
        let dropped = connector.frames_dropped();
        let now = Instant::now();
        if gate.poll(dropped, now)
            && last_kf_req.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100))
        {
            last_kf_req = Some(now);
            // The IDR repairs everything a pending RFI would have named.
            pending_rfi = None;
            let _ = connector.request_keyframe();
            tracing::debug!(
                dropped,
                "requested keyframe (loss recovery / overdue re-anchor)"
            );
        }
        flush_pending_rfi(&mut pending_rfi, &mut last_kf_req, now, &connector);

        if window_start.elapsed() >= Duration::from_secs(1) {
            let pin_kbps = connector.unsustainable_pin_kbps();
            if pin_kbps != 0 && !pin_noticed {
                pin_noticed = true;
                let _ = ev_tx.try_send(SessionEvent::Notice(format!(
                    "This device can't keep up with the pinned {} Mbps. Set the bitrate to \
                     Automatic or lower it.",
                    pin_kbps / 1000
                )));
            }
            if let Some(outcome) = connector.launch_outcome() {
                if launch_told.as_ref() != Some(&outcome) {
                    if let Some(n) = outcome.notice() {
                        let _ = ev_tx.try_send(SessionEvent::Notice(n.to_string()));
                    }
                    launch_told = Some(outcome);
                }
            }
            // ~1 Hz phase-lock report, riding the stats window. Quiet until the
            // presenter has a grid (period 0) or the window is thin (< 8 arrivals).
            // 1 ms uncertainty.
            if params.phase_lock {
                let period = latch_grid.period_ns.load(Ordering::Relaxed);
                let anchor = latch_grid.anchor_ns.load(Ordering::Relaxed);
                if period > 0 && anchor > 0 {
                    let leads_us: Vec<u64> = phase_arrivals
                        .iter()
                        .map(|a| {
                            ((anchor as i128 - *a as i128).rem_euclid(period as i128) / 1000) as u64
                        })
                        .collect();
                    if let Some((lead_ns, coherence)) =
                        punktfunk_core::phase::circular_latch(&leads_us, period as i64)
                    {
                        // Extrapolate the (possibly ~1 s old) anchor to the next latch
                        // at or after now, then express it on the host clock.
                        let (now, p, a) = (now_ns() as i128, period as i128, anchor as i128);
                        let k = ((now - a).max(0) + p - 1) / p;
                        let offset = clock_offset_live.load(Ordering::Relaxed) as i128;
                        connector.report_phase(
                            (a + k * p + offset).max(0) as u64,
                            period.min(u32::MAX as u64) as u32,
                            1_000_000,
                            lead_ns.min(u32::MAX as u64) as u32,
                            coherence,
                        );
                    }
                }
                phase_arrivals.clear();
            }
            let _ = ev_tx.try_send(SessionEvent::DecodeFacts(DecodeFacts {
                decoder: dec_path,
                health: decoder.decode_health(),
            }));
            window_start = Instant::now();
            fence_sampled = false;
        }
    };

    tracing::info!(
        total_frames,
        reason = end.as_deref().unwrap_or("user"),
        "session ended"
    );
    stop.store(true, Ordering::SeqCst);
    // About to drop the uplink — stop claiming a mute surface, so an embedder still
    // holding the handle cannot draw a muted mic that no longer exists.
    mic.set_live(false);
    if let Some(t) = audio_thread {
        let _ = t.join(); // exits within its 100 ms pull timeout once `stop` is set
    }
    if let Some(t) = pad_audio_thread {
        let _ = t.join(); // exits within its 10 ms pull timeout once `stop` is set
    }
    if let Some(t) = clipboard_thread {
        let _ = t.join(); // exits within its next_clip wait once `stop` is set
    }
    // Codec-exhaustion end is sent here, after those threads have joined, so a
    // reconnect never has two sessions' threads on the same connector.
    let _ = ev_tx.send_blocking(codec_fallback.unwrap_or(SessionEvent::Ended(end)));
}

/// Terminal event for a codec that exhausted the decode ladder, and bump telemetry.
/// One place for both refusal sites so they produce the same retry.
fn codec_fallback_event(
    negotiated: u8,
    advertised: u8,
    loss: crate::video::RungLoss,
    detail: &str,
) -> SessionEvent {
    use crate::video::{last_rung_verdict, wire_codec_name, LastRungVerdict};
    CODEC_FALLBACKS.fetch_add(1, Ordering::Relaxed);
    let codec = wire_codec_name(negotiated);
    match last_rung_verdict(negotiated, advertised, loss) {
        LastRungVerdict::Retry { caps } => {
            tracing::warn!(
                codec,
                retry_caps = caps,
                detail,
                "video decode ran out of rungs — reconnecting without this codec"
            );
            SessionEvent::CodecFallback {
                // Derived from the verdict, never from the failed codec alone: the
                // retry then advertises exactly `caps`, so the wire and the rule
                // cannot disagree.
                exclude_codecs: advertised & !caps,
                retry_caps: caps,
                msg: format!("{codec} decoding failed on this device — reconnecting"),
            }
        }
        // Nothing left to advertise: reconnecting would negotiate the same dead end.
        LastRungVerdict::Dead => {
            tracing::error!(codec, detail, "video decode ran out of rungs and of codecs");
            SessionEvent::Ended(Some(format!(
                "{codec} can't be decoded on this device, and no other codec is available"
            )))
        }
    }
}

/// Dedicated audio thread: owns decoder, scratch, and PipeWire player, and blocks
/// on `next_audio` (the plane's single consumer). Decoded chunks are Vecs recycled
/// from the player's pool — steady state allocates nothing. A muted stream still
/// decodes and still queues, in silence: the device never closes, so a toggle costs
/// neither a click nor a resync. Best-effort: setup failure logs and the session
/// streams video-only. Exits on stop or a closed plane.
fn spawn_audio(
    connector: Arc<NativeClient>,
    stop: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    // Decoder + playback from the host-resolved format, never the request. Opening
    // the device from the request is the failure a clamping host would trigger.
    let channels = connector.audio_channels;
    // A codec this client does not speak is refused out loud. `Welcome::decode`
    // takes `audio_codec` verbatim — folding an unknown id onto Opus would
    // Opus-decode a `0xD3` payload (noise) or wait forever for `0xC9` (silence).
    if !matches!(
        connector.audio_codec,
        punktfunk_core::quic::AUDIO_CODEC_OPUS | punktfunk_core::quic::AUDIO_CODEC_PCM
    ) {
        tracing::warn!(
            codec = connector.audio_codec,
            "the host resolved an audio plane this client cannot decode — streaming video-only"
        );
        return None;
    }
    let lossless = connector.audio_codec == punktfunk_core::quic::AUDIO_CODEC_PCM;
    // Same refusal for the coupling: a wrong pairing plays, and plays the wrong speakers.
    let Some(layout) = punktfunk_core::audio::AudioLayout::from_wire(connector.audio_layout) else {
        tracing::warn!(
            layout = connector.audio_layout,
            "the host resolved an audio layout this client cannot decode — streaming video-only"
        );
        return None;
    };
    // Zero is inexpressible off the wire, but everything below divides by it and
    // libopus refuses it. This is the one value that must not depend on a peer.
    let rate_hz = match connector.audio_sample_rate_hz {
        0 => punktfunk_core::audio::SAMPLE_RATE_HZ,
        hz => hz,
    };
    // One protocol frame. Opus is the fixed 5 ms; lossless negotiates from path
    // MTU, so it must be read, never assumed.
    let frame_us = if lossless {
        // Floor at the ladder's shortest rung. `0` could only come from a host that
        // did not state a duration; sizing a quantum from zero is worse than 1 ms.
        (connector.audio_frame_us as u32).max(1_000)
    } else {
        punktfunk_core::audio::FRAME_MS * 1000
    };
    tracing::info!(
        codec = if lossless { "pcm" } else { "opus" },
        channels,
        rate_hz,
        bits = connector.audio_bits,
        frame_us,
        "negotiated audio format"
    );
    let player = audio::AudioPlayer::spawn(audio::PlaybackFormat {
        channels: channels as u32,
        rate_hz,
        frame_us,
    })
    .map_err(|e| tracing::warn!(error = %e, "audio disabled"))
    .ok()?;
    let mut dec = AudioDec::new(
        connector.audio_codec,
        channels,
        rate_hz,
        connector.audio_bits,
        layout,
    )
    .map_err(|e| tracing::warn!(error = %e, "opus decoder failed — audio disabled"))
    .ok()?;
    // A/V sync. This thread holds the packet's host capture `pts_ns`, the ring
    // depth, and the video e2e figure. `PUNKTFUNK_NO_AV_SYNC` is the escape hatch.
    let av_sync_enabled = !matches!(
        std::env::var("PUNKTFUNK_NO_AV_SYNC").as_deref(),
        Ok("1") | Ok("true")
    );
    let sync_cell = player.sync_cell();
    // Device-callback counters. Logged from this thread, on wall clock — the
    // PipeWire callback runs on the graph's realtime loop and formats nothing.
    let vitals = player.vitals();
    let video_e2e = connector.video_e2e_shared();
    let av_offset_out = connector.audio_av_offset_shared();
    let buffer_ms_out = connector.audio_buffer_ms_shared();
    // Interleaved samples per ms, in the resolved rate: 48×ch at protocol default,
    // 96×ch on a 96 kHz lossless session. The old 48 kHz constant would have halved
    // every `buffer_ms` this thread publishes, in the direction that looks healthy.
    let per_ms = (rate_hz / 1000).max(1) as usize * channels.max(1) as usize;
    // Decode scratch. Opus: up to 120 ms (hard bound — libopus decodes into a
    // fixed slice), derived from the rate so it cannot silently become 60 ms.
    // PCM: one negotiated frame; `pcm::to_f32` grows the Vec, so oversized
    // datagrams reallocate rather than overrun.
    let scratch = if lossless {
        punktfunk_core::audio::pcm::samples_per_frame(rate_hz, frame_us, channels)
    } else {
        120 * per_ms
    };
    // Pull-loop tick, one protocol frame. Rounded up so a sub-millisecond rung can
    // never round to a zero-length timeout and spin.
    let frame_ms = (frame_us as u64).div_ceil(1000).max(1);
    std::thread::Builder::new()
        .name("punktfunk-audio-rx".into())
        .spawn(move || {
            // Best-effort priority for the decode leg. Lateness is absorbed by the
            // ring, but a thread descheduled past ring depth is a drought. Refusal
            // leaves the thread as it was (`audio_rt`).
            crate::audio_rt::boost_and_log("punktfunk-audio-rx");
            let mut pcm = vec![0f32; scratch];
            // Every frame reaches the device through here. A muted stream queues silence of
            // the same length, so the ring keeps its cadence and unmute lands in step — the
            // frame still decoded, so there is no decoder state to rebuild.
            let queue = |player: &audio::AudioPlayer, pcm: &[f32]| {
                let mut buf = player.take_buffer();
                if connector.audio_muted() {
                    buf.resize(pcm.len(), 0.0);
                } else {
                    buf.extend_from_slice(pcm);
                }
                player.push(buf);
            };
            let mut gaps = punktfunk_core::audio::AudioGapTracker::new_at_frame_us(frame_us);
            let mut frame_samples = 0usize;
            let mut av = punktfunk_core::audio::AvSync::new_at_rate(channels, rate_hz);
            if !av_sync_enabled {
                tracing::info!("A/V sync disabled by PUNKTFUNK_NO_AV_SYNC");
            }
            // Drought concealment. A seq gap is concealed when a later packet
            // arrives; when the wire goes quiet nothing arrives to reveal it.
            // Told this plane's real frame so the fuse is spent at this session's pace.
            let mut drought = punktfunk_core::audio::DroughtConceal::new_at_frame_us(
                audio::TUNING.plc_max_ms(),
                frame_us,
            );
            let mut last_packet = std::time::Instant::now();
            // Playback vitals ~every 10 s on wall clock, plus a one-shot quantum
            // line the first time the callback has published one.
            let mut last_vitals = std::time::Instant::now();
            let mut quantum_logged = false;
            while !stop.load(Ordering::SeqCst) {
                if !quantum_logged && vitals.quantum_known() {
                    quantum_logged = true;
                    let v = vitals.snapshot();
                    tracing::info!(
                        requested_frames = v.requested_frames,
                        capacity_frames = v.capacity_frames,
                        write_frames = v.write_frames,
                        // From the session's rate, not from 48: a 96 kHz quantum
                        // divided by 48 reads as twice the latency it is.
                        write_ms = v.write_frames / (rate_hz / 1000).max(1),
                        rate_hz,
                        "audio playback quantum"
                    );
                }
                if last_vitals.elapsed() >= Duration::from_secs(10) {
                    last_vitals = std::time::Instant::now();
                    let v = vitals.snapshot();
                    tracing::debug!(
                        buffer_ms = v.buffer_ms,
                        target_ms = v.target_ms,
                        underruns = v.underruns,
                        drift_sheds = v.sheds,
                        // Sync-driven deepening, one duplicated crossfaded frame each.
                        drift_inserts = v.inserts,
                        callbacks = v.callbacks,
                        // Concealment next to the underruns it prevented: a healthy
                        // `underruns` bought with climbing `plc_ms` is a link in trouble.
                        plc_ms = sync_cell.plc_ms(),
                        "audio playback"
                    );
                }
                // Wait at most one frame while there is a stream to protect. Before
                // anything has decoded, a session whose host never sends audio keeps
                // the long timeout rather than waking 200 times a second to do nothing.
                let wait_ms = if frame_samples > 0 { frame_ms } else { 100 };
                match connector.next_audio(Duration::from_millis(wait_ms)) {
                    Ok(pkt) => {
                        // Place this frame against the picture before it is queued:
                        // `buffered_ahead` is everything that must still play first.
                        let depth = sync_cell.depth();
                        // Published even with sync off — ring depth is what makes a
                        // "too much latency" report triageable.
                        buffer_ms_out.store((depth / per_ms) as u32, Ordering::Relaxed);
                        if av_sync_enabled {
                            let ve2e = video_e2e.load(Ordering::Relaxed);
                            let o = punktfunk_core::audio::AvSyncObservation {
                                pts_ns: pkt.pts_ns,
                                now_local_ns: punktfunk_core::client::now_realtime_ns(),
                                clock_offset_ns: connector.clock_offset_now_ns(),
                                buffered_ahead: depth,
                                output_latency_ns: sync_cell.output_latency_ns(),
                                // 0 = nothing on the glass yet; no reference, no correction.
                                video_e2e_ns: (ve2e > 0).then_some(ve2e),
                            };
                            av.observe(o);
                            sync_cell.set_target(av.desired_depth(depth));
                            av_offset_out.store(av.offset_ms() as i64, Ordering::Relaxed);
                        }
                        last_packet = std::time::Instant::now();
                        // Anything the drought path already covered is audio the stream
                        // now has; concealing it again would insert samples it never
                        // carried and push everything after them later.
                        let already = drought.packet();
                        // Conceal a seq gap before decoding the arrival. Opus interpolates
                        // from decoder state; PCM repeats-and-fades — lossless has nothing
                        // to interpolate from. Gap arithmetic is codec-independent.
                        for _ in 0..gaps.missing_before(pkt.seq).saturating_sub(already) {
                            if frame_samples == 0 {
                                break;
                            }
                            if let Some(n) = dec.conceal(frame_samples, &mut pcm) {
                                queue(&player, &pcm[..n]);
                            }
                        }
                        match dec.decode(&pkt.data, &mut pcm) {
                            Some(n) => {
                                frame_samples = n;
                                queue(&player, &pcm[..n]);
                            }
                            // Opus: corrupt packet. PCM: not a whole number of samples
                            // at the negotiated depth. Either way the frame is lost.
                            None => tracing::debug!(bytes = pkt.data.len(), "audio decode failed"),
                        }
                    }
                    Err(PunktfunkError::NoFrame) => {
                        // Nothing on the wire. If the ring is draining, conceal at one
                        // frame per tick — this arm fires every frame time, the rate
                        // the callback drains at. `frame_samples` is 0 until first decode.
                        let depth_ms = (sync_cell.depth() / per_ms) as u32;
                        if frame_samples > 0 && drought.conceal(last_packet.elapsed(), depth_ms) {
                            if let Some(n) = dec.conceal(frame_samples, &mut pcm) {
                                queue(&player, &pcm[..n]);
                            }
                            sync_cell.publish_plc_ms(drought.total_ms());
                        }
                    }
                    Err(_) => break,
                }
            }
            tracing::debug!("audio pull thread exited");
        })
        .map_err(|e| tracing::warn!(error = %e, "audio thread start failed — audio disabled"))
        .ok()
}

/// `PUNKTFUNK_DEBUG_RECONFIGURE`: `WxH@HZ:SECS` → request that mode SECS seconds in.
fn parse_debug_reconfigure(s: &str) -> Option<(Mode, Duration)> {
    let (mode_s, secs_s) = s.split_once(':')?;
    let (res, hz) = mode_s.split_once('@')?;
    let (w, h) = res.split_once('x')?;
    let mode = Mode {
        width: w.trim().parse().ok()?,
        height: h.trim().parse().ok()?,
        refresh_hz: hz.trim().parse().ok()?,
    };
    Some((mode, Duration::from_secs(secs_s.trim().parse().ok()?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every spelling the env-var doc promises has to land on the right side of
    /// `CLIENT_CAP_AUDIO_HIRES`.
    #[test]
    fn the_hires_opt_in_parses_the_spellings_it_documents() {
        use punktfunk_core::audio::pcm::{BITS_16, BITS_24};
        for off in ["", "0", "off", "false", "no", "  Off  "] {
            assert_eq!(parse_audio_format(off), AudioRequest::Legacy, "{off:?}");
        }
        // On → the flagship rung. 24-bit, because 16-bit PCM spends 1.5 Mbps to
        // sound like transparent Opus.
        for on in ["1", "on", "true", "YES"] {
            assert_eq!(
                parse_audio_format(on),
                AudioRequest::Hires(96_000, BITS_24),
                "{on:?}"
            );
        }
        assert_eq!(
            parse_audio_format("96000"),
            AudioRequest::Hires(96_000, BITS_24)
        );
        assert_eq!(
            parse_audio_format("48000/24"),
            AudioRequest::Hires(48_000, BITS_24)
        );
        assert_eq!(
            parse_audio_format("96000/16"),
            AudioRequest::Hires(96_000, BITS_16)
        );
        // 48 kHz/16-bit is a request, not "off" — cheapest lossless rung. Connect
        // turns this into explicit `48000`/`16` and `Legacy` into unspecified `0`/`0`.
        assert_eq!(
            parse_audio_format("48000/16"),
            AudioRequest::Hires(48_000, BITS_16)
        );
        assert_ne!(parse_audio_format("48000/16"), AudioRequest::Legacy);
        // "Not asking" must never reach the wire as a format, or every ordinary
        // session would advertise hi-res.
        assert_eq!(AUDIO_FORMAT_UNSPECIFIED, (0, 0));
    }

    /// Refuse rungs the plane cannot carry here, where the user can be told.
    /// 44.1 kHz looks reasonable; it truncates `JitterPolicy`'s integer samples-per-ms.
    #[test]
    fn the_hires_opt_in_refuses_what_the_plane_cannot_carry() {
        for bad in [
            "44100",
            "44100/24",
            "88200/24",
            "192000/24",
            "48000/32",
            "96000/8",
            "96000/",
            "/24",
            "96 kHz",
            "yes please",
            "-96000/24",
        ] {
            assert_eq!(
                parse_audio_format(bad),
                AudioRequest::Unsupported,
                "{bad:?} should not parse"
            );
        }
    }

    /// Stored values and the pair each asks for. Spellings are shared with Apple
    /// `AudioFormatChoice` and Android `AUDIO_FORMAT_*`, so a typo here is ignored
    /// on this client while the preset keeps working on the others.
    #[test]
    fn the_audio_format_setting_speaks_the_cross_client_spellings() {
        use punktfunk_core::audio::pcm::BITS_24;
        assert_eq!(AUDIO_FORMAT_OPUS, "opus");
        assert_eq!(AUDIO_FORMAT_LOSSLESS_48, "lossless48");
        assert_eq!(AUDIO_FORMAT_LOSSLESS_96, "lossless96");
        assert_eq!(
            AUDIO_FORMATS.iter().map(|(v, _)| *v).collect::<Vec<_>>(),
            [
                AUDIO_FORMAT_OPUS,
                AUDIO_FORMAT_LOSSLESS_48,
                AUDIO_FORMAT_LOSSLESS_96
            ]
        );

        assert_eq!(audio_format_wire(AUDIO_FORMAT_OPUS), None);
        // Both lossless rows are 24-bit: 16-bit PCM would spend 1.5 Mbps to sound
        // like the 256 kbps Opus it replaces, which is why no row offers it.
        assert_eq!(
            audio_format_wire(AUDIO_FORMAT_LOSSLESS_48),
            Some((48_000, BITS_24))
        );
        assert_eq!(
            audio_format_wire(AUDIO_FORMAT_LOSSLESS_96),
            Some((96_000, BITS_24))
        );
        // A newer client's row, and a corrupted store: Opus, never a refused connect.
        assert_eq!(audio_format_wire("lossless192"), None);
        assert_eq!(audio_format_wire(""), None);
    }

    /// `PUNKTFUNK_AUDIO_HIRES` overrides the setting in both directions. Unset leaves
    /// the setting alone. A typo is ignored rather than read as "off" — that would
    /// silently defeat a UI choice.
    #[test]
    fn the_env_override_beats_the_setting_in_both_directions() {
        use punktfunk_core::audio::pcm::{BITS_16, BITS_24};

        assert_eq!(resolve_audio_format(None, AUDIO_FORMAT_OPUS), None);
        assert_eq!(
            resolve_audio_format(None, AUDIO_FORMAT_LOSSLESS_96),
            Some((96_000, BITS_24))
        );

        // Set: it wins, including turning a lossless setting off.
        assert_eq!(
            resolve_audio_format(Some("1"), AUDIO_FORMAT_OPUS),
            Some((96_000, BITS_24))
        );
        assert_eq!(
            resolve_audio_format(Some("48000/16"), AUDIO_FORMAT_LOSSLESS_96),
            Some((48_000, BITS_16)),
            "the env rung the menu does not offer is still reachable"
        );
        for off in ["0", "off", "false"] {
            assert_eq!(
                resolve_audio_format(Some(off), AUDIO_FORMAT_LOSSLESS_96),
                None,
                "{off:?} must force Opus over a lossless setting"
            );
        }

        assert_eq!(
            resolve_audio_format(Some("96 kHz"), AUDIO_FORMAT_LOSSLESS_48),
            Some((48_000, BITS_24))
        );
        assert_eq!(resolve_audio_format(Some("44100"), AUDIO_FORMAT_OPUS), None);
    }

    /// Lossless arm: interleaved counts, concealment says no before it has anything
    /// to repeat, truncated datagram refused. A per-channel mix-up would halve every
    /// ring push — audible as a starving ring, not an obvious failure.
    #[test]
    fn the_lossless_plane_decodes_and_conceals_in_interleaved_samples() {
        use punktfunk_core::audio::pcm;
        let mut dec = AudioDec::new(
            punktfunk_core::quic::AUDIO_CODEC_PCM,
            2,
            96_000,
            pcm::BITS_24,
            punktfunk_core::audio::AudioLayout::Legacy,
        )
        .expect("the PCM arm builds no codec and cannot fail");
        let mut out = Vec::new();
        // Saying so makes the caller emit silence and let the ring re-prime, instead
        // of playing an uninitialised buffer.
        assert_eq!(dec.conceal(384, &mut out), None);

        // 2 ms frame at 96 kHz/24-bit stereo — the rung the default MTU ceiling lands on.
        let frame = pcm::samples_per_frame(96_000, 2_000, 2);
        assert_eq!(frame, 384, "192 samples per channel, interleaved");
        let mut wire = Vec::new();
        pcm::from_f32(&vec![0.5f32; frame], pcm::BITS_24, &mut wire);
        assert_eq!(
            dec.decode(&wire, &mut out),
            Some(frame),
            "interleaved count"
        );
        assert!(out[..frame].iter().all(|s| (s - 0.5).abs() < 1e-3));

        // `PcmConceal` holds the frame it repeats, at the frame's own length.
        assert_eq!(dec.conceal(0, &mut out), Some(frame));

        // Not a whole number of samples at the negotiated depth: refuse rather than
        // decode a shifted frame.
        assert_eq!(dec.decode(&wire[..wire.len() - 1], &mut out), None);
    }

    /// Opus arm through the same methods: they return interleaved counts where
    /// libopus counts per channel — the one place this could have halved a working plane.
    #[test]
    fn the_opus_plane_reports_interleaved_samples_too() {
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
            .expect("opus encoder");
        let mut packet = [0u8; 4_000];
        let silence = [0.0f32; 240 * 2];
        let n = enc
            .encode_float(&silence, &mut packet)
            .expect("encode one 5 ms stereo frame");
        let mut dec = AudioDec::new(
            punktfunk_core::quic::AUDIO_CODEC_OPUS,
            2,
            48_000,
            16,
            punktfunk_core::audio::AudioLayout::Legacy,
        )
        .expect("opus decoder");
        // Pump scratch: 120 ms — the biggest frame the Opus plane can carry.
        let mut out = vec![0f32; 120 * 48 * 2];
        assert_eq!(dec.decode(&packet[..n], &mut out), Some(240 * 2));
        // PLC is asked for, and answered, in the same unit.
        assert_eq!(dec.conceal(240 * 2, &mut out), Some(240 * 2));
        // Nothing to size PLC from is a `None`, not a panic on an empty slice.
        let mut empty = Vec::new();
        assert_eq!(dec.conceal(0, &mut empty), None);
    }

    #[test]
    fn debug_reconfigure_parses_the_documented_shape() {
        let (mode, delay) = parse_debug_reconfigure("1280x720@60:5").unwrap();
        assert_eq!((mode.width, mode.height, mode.refresh_hz), (1280, 720, 60));
        assert_eq!(delay, Duration::from_secs(5));
    }

    #[test]
    fn debug_reconfigure_rejects_garbage() {
        for bad in [
            "",
            "1280x720",
            "1280x720@60",
            "x@:",
            "ax b@c:d",
            "1280x720@60:x",
        ] {
            assert!(parse_debug_reconfigure(bad).is_none(), "{bad:?} parsed");
        }
    }

    /// Mute is inert until the pump reports a live uplink.
    #[test]
    fn mic_mute_is_a_no_op_without_an_uplink() {
        let mic = MicControl::default();
        assert!(!mic.live());
        assert_eq!(mic.toggle(), None, "no uplink, nothing to toggle");
        assert!(!mic.muted(), "and nothing to show");

        mic.set_live(true);
        assert_eq!(mic.toggle(), Some(true));
        assert!(mic.muted());
        // Capture side reads the same flag the toggle writes.
        assert!(mic.flag().load(Ordering::Relaxed));
        assert_eq!(mic.toggle(), Some(false));
        assert!(!mic.muted());

        // A mute that outlives its uplink stops being shown (session end clears `live`).
        assert_eq!(mic.toggle(), Some(true));
        mic.set_live(false);
        assert!(!mic.muted());
        assert_eq!(mic.toggle(), None);
    }

    /// HEVC reconnect as the terminal event both refusal sites produce. Pins that
    /// the retry never re-offers the failed codec, the message is user-facing, and
    /// the counter moves once per occurrence.
    ///
    /// `CODEC_FALLBACKS` is process-global, so tests that bump it take this lock.
    static FALLBACK_COUNTER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn an_exhausted_codec_produces_a_retry_event_and_moves_the_counter() {
        use crate::video::RungLoss;
        use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC};
        let _guard = FALLBACK_COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        let before = codec_fallbacks();

        let ev = codec_fallback_event(
            CODEC_HEVC,
            CODEC_H264 | CODEC_HEVC,
            RungLoss::Codec,
            "no software HEVC",
        );
        match ev {
            SessionEvent::CodecFallback {
                exclude_codecs,
                retry_caps,
                ref msg,
            } => {
                assert_eq!(exclude_codecs, CODEC_HEVC, "the retry must drop HEVC");
                assert_eq!(retry_caps, CODEC_H264);
                // Toast for a person: names the codec and says what happens next.
                assert!(msg.contains("HEVC"), "{msg}");
                assert!(msg.contains("reconnect"), "{msg}");
            }
            _ => panic!("expected a CodecFallback"),
        }
        assert_eq!(codec_fallbacks(), before + 1, "counted exactly once");

        match codec_fallback_event(
            CODEC_HEVC,
            CODEC_H264 | CODEC_HEVC | CODEC_AV1,
            RungLoss::Codec,
            "x",
        ) {
            SessionEvent::CodecFallback { retry_caps, .. } => {
                assert_eq!(retry_caps, CODEC_H264 | CODEC_AV1);
            }
            _ => panic!("expected a CodecFallback"),
        }

        // Nothing left to offer: end honestly instead of a reconnect loop. Still
        // counted — the failure happened.
        let before = codec_fallbacks();
        match codec_fallback_event(CODEC_HEVC, CODEC_HEVC, RungLoss::Codec, "x") {
            SessionEvent::Ended(Some(msg)) => {
                assert!(msg.contains("HEVC"), "{msg}");
                assert!(msg.contains("no other codec"), "{msg}");
            }
            _ => panic!("expected a plain Ended"),
        }
        assert_eq!(codec_fallbacks(), before + 1);
    }

    /// `exclude_codecs` and `retry_caps` describe the same retry. The wire follows
    /// `exclude_codecs`, so a mismatch means the tested rule is not the shipped one.
    /// Re-intersecting `advertised & !exclude` must land on `retry_caps`.
    #[test]
    fn the_retrys_exclusion_resolves_to_exactly_its_advertised_caps() {
        use crate::video::RungLoss;
        use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE};
        let _guard = FALLBACK_COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        for advertised in 0u8..16 {
            for negotiated in [CODEC_H264, CODEC_HEVC, CODEC_AV1, CODEC_PYROWAVE] {
                for loss in [RungLoss::Codec, RungLoss::Shape] {
                    let SessionEvent::CodecFallback {
                        exclude_codecs,
                        retry_caps,
                        ..
                    } = codec_fallback_event(negotiated, advertised, loss, "x")
                    else {
                        continue; // Dead — nothing is advertised at all
                    };
                    assert_eq!(
                        advertised & !exclude_codecs,
                        retry_caps,
                        "advertised {advertised:#x} negotiated {negotiated:#x} {loss:?}"
                    );
                    assert_eq!(retry_caps & negotiated, 0, "the failed codec came back");
                }
            }
        }
        // Excluding twice is idempotent — a second fallback ORs into the existing value.
        let full = CODEC_H264 | CODEC_HEVC | CODEC_AV1;
        assert_eq!((full & !CODEC_HEVC) & !CODEC_HEVC, CODEC_H264 | CODEC_AV1);
    }
}
