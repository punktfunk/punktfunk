//! Native `punktfunk/1` Hello → Welcome → Start negotiation.
//!
//! Pairing stays in `serve_session`: the delegated-approval wait must outlive this handshake
//! timeout and must release the session permit. This module then admits the client, negotiates
//! encode / audio / cursor, binds the data-plane UDP socket, and returns the values
//! `serve_session` needs to stand the session up.
//!
//! Evidence: `design/hi-res-audio.md`, `design/remote-desktop-sweep.md`.

use super::*;

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
/// Produced together by [`resolve_audio_plane`] so the two cannot disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AudioPlane {
    /// [`AUDIO_CODEC_OPUS`](punktfunk_core::quic::AUDIO_CODEC_OPUS) or
    /// [`AUDIO_CODEC_PCM`](punktfunk_core::quic::AUDIO_CODEC_PCM).
    pub codec: u8,
    pub rate_hz: u32,
    pub bits: u8,
    /// Frame duration in µs on the PCM plane; `0` on Opus (`0xC9` is a fixed 5 ms).
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

/// Max share of the session video bitrate the hi-res plane may take.
///
/// Audio is outside ABR, so this is not "video adapts around 25 %". Separate from
/// [`plan_audio_budget`](punktfunk_core::audio::plan_audio_budget)'s 5 % Opus ladder: hi-res
/// is opt-in, never a new rung. Stereo 48/16 (1 536 kbps) needs ≥ 6.1 Mbps of video;
/// 176.4/24 7.1 needs ≥ 33.9.
const HIRES_MAX_VIDEO_SHARE_PCT: u32 = 25;

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

/// Out-of-band cursor: client asked, capture can deliver metadata, encoder can blend.
///
/// Welcome `HOST_CAP_CURSOR` is computed from this; session wiring reads that bit back.
/// Denied, the host still composites wherever the backend can blend.
pub(super) fn cursor_forward(
    client_caps: u8,
    compositor: Option<crate::vdisplay::Compositor>,
    codec: crate::encode::Codec,
    bit_depth: u8,
) -> bool {
    if client_caps & punktfunk_core::quic::CLIENT_CAP_CURSOR == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        // Same CUDA prediction `SessionPlan` makes: NVENC blends a CUDA payload only.
        let cuda_planned = !crate::encode::linux_zero_copy_is_vaapi() && crate::zerocopy::enabled();
        compositor.is_some_and(|c| c != crate::vdisplay::Compositor::Gamescope)
            && crate::encode::cursor_blend_capable(codec, cuda_planned, bit_depth == 10)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Windows: the v5 IddCx hardware-cursor channel. Without it DWM paints the pointer
        // into the IDD frame and a second copy doubles it. The encoder is not consulted: the
        // IDD capturer composites on the capture-mouse flip; no Windows encode backend blends.
        let _ = (compositor, codec, bit_depth);
        crate::windows::idd::hw_cursor_capable()
    }
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

/// Hello → Welcome → Start. Borrows the control streams; the caller keeps them for mid-stream
/// renegotiation. `first` is the already-read first control message.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub(super) async fn negotiate(
    conn: &super::link::SessionLink,
    send: &mut super::link::CtlSend,
    recv: &mut super::link::CtlRecv,
    first: &[u8],
    source: Punktfunk1Source,
    frames: u32,
    // `Some(port request)` binds a UDP data socket; `None` is a browser, whose video rides the
    // connection it is already on and which is told `udp_port: 0`.
    data_port: Option<Option<u16>>,
    // `welcome` / `start` stamps; Welcome-time display prep threads this into pipeline-build.
    bringup: &Arc<crate::bringup::Trace>,
    // Created before the handshake so Welcome-time display prep sees a vanished client
    // (`stop` aborts a build retry; `quit` rides into the display lease).
    quit: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    // Effective grant mask and seconds until expiry (`0` = permanent), resolved at admission.
    grants: u32,
    expires_in_secs: u32,
) -> Result<(
    Hello,
    Welcome,
    u16,
    Option<std::net::UdpSocket>,
    Start,
    // What the client calls itself (`EXT_TAG_CLIENT` on `Start`); `None` from one that sent no
    // block. Log only: two dialers from one device are told apart by that line.
    Option<String>,
    Option<crate::vdisplay::Compositor>,
    // Gamescope sub-mode as a value, not process env — a concurrent connect would overwrite env.
    Option<crate::vdisplay::GamescopeRoute>,
    Option<super::stream::PrepHandle>,
    // Admitted by `mode_conflict: join`: the owner's display, which this session shares, and
    // the size this client asked for.
    Option<(crate::vdisplay::admission::LiveDisplay, (u32, u32))>,
)> {
    let mut hello = Hello::decode(first).map_err(|e| anyhow!("Hello decode: {e:?}"))?;
    if hello.abi_version != punktfunk_core::WIRE_VERSION {
        close_rejected(
            conn,
            punktfunk_core::reject::RejectReason::WireVersionMismatch,
        );
        anyhow::bail!(
            "wire version mismatch: client {} host {}",
            hello.abi_version,
            punktfunk_core::WIRE_VERSION
        );
    }
    // Pairing ran before this future: a client here is paired, or the host is `--open`.

    // GPU-probed host codecs ∩ client advertised, honoring preference. A software host is
    // H.264-only — refuse rather than send a stream an HEVC-only client cannot decode.
    let host_codecs = crate::encode::host_wire_caps();
    let codec_bit =
            punktfunk_core::quic::resolve_codec(hello.video_codecs, host_codecs, hello.preferred_codec)
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

    // The operator's cap for this device, applied BEFORE mode-conflict and before Welcome
    // — the client is then told the mode it actually gets, rather than asking for 4K120,
    // being told yes, and receiving something else (§6.3 P2).
    {
        let fp = crate::vdisplay::policy::fp_hex(conn.peer_fingerprint());
        let want = (hello.mode.width, hello.mode.height, hello.mode.refresh_hz);
        let capped = crate::vdisplay::policy::prefs()
            .get()
            .cap_mode(fp.as_deref(), want);
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

    // Mode-conflict before Welcome. Same-client reconnect never conflicts. This session
    // registers in the live set only once its data plane is up, so a later client can steal it.
    let mut joined = None;
    {
        use crate::vdisplay::admission::{admit, preempt_same_identity, Admission};
        let peer_fp = conn.peer_fingerprint();

        // Own prior session (QUIC idle has not fired). Stop it and wait the release grace so
        // this reconnect reuses the kept display. Runs before we register, so we never stop ourselves.
        let own_zombies = preempt_same_identity(peer_fp);
        if !own_zombies.is_empty() {
            tracing::info!(
                    count = own_zombies.len(),
                    "reconnect: preempting this client's own zombie session(s) so the kept display is reused"
                );
            for z in &own_zombies {
                z.store(true, Ordering::SeqCst);
            }
            // 1500 ms: same release grace as steal, so the zombie drops its display before we acquire.
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }

        match admit(peer_fp) {
            Admission::Separate => {}
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
                joined = Some((display, view));
            }
            Admission::Steal(victims) => {
                tracing::info!(
                    victims = victims.len(),
                    "mode-conflict: STEAL — preempting the live session(s)"
                );
                for v in &victims {
                    v.store(true, Ordering::SeqCst);
                }
                // 1500 ms release grace so victims drop their display before we acquire.
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            }
            Admission::Reject(reason) => {
                tracing::warn!("mode-conflict: REJECT — {reason}");
                // Typed refusal: BUSY + reason bytes. The client reads `ApplicationClosed`,
                // not a bare drop, so the UI can name the live session.
                conn.close(REJECT_BUSY_CODE, reason.as_bytes());
                anyhow::bail!("{reason}");
            }
        }
    }

    crate::encode::validate_dimensions(codec, hello.mode.width, hello.mode.height)
        .context("client-requested mode")?;
    crate::encode::validate_refresh(hello.mode.refresh_hz).context("client-requested mode")?;

    let (mut compositor, mut gamescope_route) =
        negotiate_compositor(source, &hello, conn.peer_fingerprint()).await?;
    // A joiner streams the owner's display, so it runs on the owner's compositor and route.
    if let Some((d, _)) = joined.as_ref().filter(|(d, _)| d.compositor.is_some()) {
        compositor = d.compositor;
        gamescope_route = d.route.clone();
    }

    // Library launch is resolved after Welcome into `SessionContext.launch`. Do not write
    // `PUNKTFUNK_GAMESCOPE_APP`: process env races concurrent sessions and only gamescope's bare spawn reads it.

    // Env/cfg only; pads are created lazily by the input thread.
    let gamepad = resolve_gamepad(hello.gamepad);

    // Capturer opens at this count (PipeWire pads silence; WASAPI AUTOCONVERTPCM up/downmixes).
    // Welcome echoes the value the audio thread will encode.
    let audio_channels = resolve_audio_channels(hello.audio_channels);
    tracing::info!(
        requested = hello.audio_channels,
        resolved = audio_channels,
        "audio channels"
    );

    let (bit_depth, session_hdr, chroma) =
        negotiate_video_format(&hello, codec, compositor).await?;

    // After depth + chroma: PyroWave Automatic is a ~bpp pin that scales with both.
    let bitrate_kbps =
        resolve_bitrate_kbps_for(codec, hello.bitrate_kbps, &hello.mode, chroma, bit_depth);
    tracing::info!(
        requested_kbps = hello.bitrate_kbps,
        resolved_kbps = bitrate_kbps,
        "encoder bitrate"
    );

    // Hold the socket through streaming — no bind→read→drop→rebind race on a fixed port.
    // Bound to this connection's local IP, not wildcard: the client accepts video only from
    // the host IP it dialed.
    let (data_sock, udp_port) = match data_port {
        Some(port) => {
            let sock = bind_data_socket(port, conn.local_ip())?;
            let udp_port = sock.local_addr()?.port();
            (Some(sock), udp_port)
        }
        // A browser: nothing to bind, nothing to punch, no second port to name.
        None => (None, 0),
    };

    // Before Welcome: a path a previous session proved jumbo is given a bounded moment to
    // re-prove itself on this connection (`negotiated_shard_payload` awaits that).
    let mut shard_payload = wire_mtu::negotiated_shard_payload(conn, hello.max_shard_payload).await;
    // A browser's video rides this connection's datagrams, which are smaller than a UDP payload
    // (QUIC and HTTP/3 framing come out of the same budget). Ask, falling back to the 1200 every
    // QUIC path guarantees; a shard that does not fit is dropped at send, and FEC cannot cover all.
    if data_port.is_none() {
        let budget = conn.max_datagram_size().unwrap_or(1200);
        shard_payload = shard_payload.min(punktfunk_core::config::shard_payload_for_udp_budget(
            budget,
            conn.remote_address().ip(),
        ));
    }

    let mut key = [0u8; 16];
    rand::rng().fill_bytes(&mut key);
    // Fresh salt with the fresh key. GCM needs only one unique; a constant salt would make
    // key-reuse catastrophic. Negotiated in Welcome.
    let mut salt = [0u8; 4];
    rand::rng().fill_bytes(&mut salt);
    // ChaCha20 when the client asked (`VIDEO_CAP_CHACHA20`, soft-AES armv7) and the operator
    // kill-switch allows. The 16-byte `key` stays independently random so nothing sees zeros.
    let client_wants_chacha = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_CHACHA20 != 0;
    let chacha = client_wants_chacha && pf_host_config::config().chacha20;
    let key_chacha = chacha.then(|| {
        let mut k = [0u8; 32];
        rand::rng().fill_bytes(&mut k);
        k
    });
    tracing::info!(
        cipher = if chacha {
            "chacha20-poly1305"
        } else {
            "aes-128-gcm"
        },
        client_wants_chacha,
        "session cipher"
    );

    let mut audio_plane = negotiate_audio_plane(conn, &hello, audio_channels, bitrate_kbps).await?;
    // The coupling the client asked for, when this host knows it. Anything else is answered as
    // legacy, which is also what an older client's absent byte means.
    audio_plane.layout = match punktfunk_core::audio::AudioLayout::from_wire(hello.audio_layout) {
        Some(layout) => layout,
        None => {
            tracing::info!(
                requested = hello.audio_layout,
                "audio layout unknown to this host — encoding legacy"
            );
            punktfunk_core::audio::AudioLayout::Legacy
        }
    };

    let welcome = Welcome {
        abi_version: punktfunk_core::WIRE_VERSION,
        udp_port,
        mode: hello.mode,
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            // Static override pins it; otherwise start at the adaptive midpoint and resize from LossReports.
            fec_percent: fec_static_override().unwrap_or(FEC_ADAPTIVE_START),
            max_data_per_block: 4096,
        },
        // Largest even payload whose sealed datagram fits an unfragmented UDP packet on a
        // 1500 MTU for this client's address family (1408 IPv4, 1388 IPv6 — v6 routers do
        // not fragment). Order: jumbo re-proof, `PUNKTFUNK_WIRE_MTU`, a prior session's
        // learned path budget, then this family default. See `wire_mtu.rs`.
        shard_payload: shard_payload as u16,
        encrypt: true,
        key,
        salt,
        frames: match source {
            Punktfunk1Source::Synthetic => frames,
            // Unbounded; the client streams until we close.
            Punktfunk1Source::SyntheticAbr(..)
            | Punktfunk1Source::Virtual
            | Punktfunk1Source::Software => 0,
        },
        // Auto for the synthetic source (no compositor).
        compositor: compositor
            .map(|c| c.as_pref())
            .unwrap_or(CompositorPref::Auto),
        gamepad,
        bitrate_kbps,
        bit_depth,
        // HDR verdict, not bit depth: 10-bit SDR is Main10 under BT.709 and must say SDR.
        // Mastering metadata (ST.2086 + CLL) rides the 0xCE datagram.
        color: if session_hdr {
            ColorInfo::HDR10_BT2020_PQ
        } else {
            ColorInfo::SDR_BT709
        },
        chroma_format: crate::encode::chroma_idc(chroma),
        audio_channels,
        // Negotiated codec; the client must not assume HEVC.
        codec: codec_bit,
        // Sequence-gated gamepad snapshots; capable clients send those, not per-transition events.
        // Clipboard only when operator policy and a platform backend both exist.
        host_caps: punktfunk_core::quic::HOST_CAP_GAMEPAD_STATE
            | if pf_clipboard::cap_advertised() {
                punktfunk_core::quic::HOST_CAP_CLIPBOARD
            } else {
                0
            }
            // Text injection only where the backend can type (SendInput UNICODE; wlroots keymap).
            // Clients without the bit keep VK-synthesis for IME.
            | if crate::inject::text_input_supported() {
                punktfunk_core::quic::HOST_CAP_TEXT_INPUT
            } else {
                0
            }
            // Client turns its local renderer on only when it sees this bit; serve_session
            // wires forwarding by reading the bit back.
            | if cursor_forward(hello.client_caps, compositor, codec, bit_depth) {
                punktfunk_core::quic::HOST_CAP_CURSOR
            } else {
                0
            }
            // Pen batches → per-session uinput tablet. Without the bit, clients fold pen into
            // touch/pointer and `NativeClient::send_pen` refuses.
            | if crate::inject::pen_supported() {
                punktfunk_core::quic::HOST_CAP_PEN
            } else {
                0
            }
            // `0xD2` only when offered, budgeted, and not PCM: it is undefined on `0xD3` and
            // the client has no PCM-side decoder for it. Stated here, not left to the audio thread.
            | if !audio_plane.is_pcm()
                && audio_budget(
                    redundancy_offered(hello.client_caps),
                    bitrate_kbps,
                    audio_channels,
                    audio_plane.layout,
                )
                .redundancy
            {
                punktfunk_core::quic::HOST_CAP_AUDIO_RED
            } else {
                0
            }
            | if super::pad_audio::host_cap(hello.client_caps) {
                punktfunk_core::quic::HOST_CAP_PAD_AUDIO
            } else {
                0
            }
            // Set only when the gate resolved to PCM, not "host could". `0x80` is the last
            // `host_caps` bit; the next capability needs a second byte and an ABI bump.
            | if audio_plane.is_pcm() {
                punktfunk_core::quic::HOST_CAP_AUDIO_HIRES
            } else {
                0
            },
        // `0` on the standalone binary (no management API); the client then keeps its default.
        mgmt_port: crate::mgmt::effective_port(),
        // Mask and remaining lifetime from admission. Full-control permanent is `GRANT_ALL, 0`.
        grants,
        expires_in_secs,
        // Cipher 0 keeps Welcome byte-identical to the pre-cipher form, unless a mgmt port
        // forces the placeholder (`Welcome::encode`). Data plane reads `welcome.session_config`.
        cipher: if chacha {
            punktfunk_core::quic::CIPHER_CHACHA20_POLY1305
        } else {
            punktfunk_core::quic::CIPHER_AES_128_GCM
        },
        key_chacha,
        // Resolved plane. Opus 48 kHz / 16-bit makes `Welcome::encode` omit the four fields
        // so the Welcome stays byte-identical to the pre-hi-res form. Client opens from these,
        // never from what it asked. `audio_frame_us` is `0` on Opus (fixed 5 ms).
        audio_codec: audio_plane.codec,
        audio_rate_hz: audio_plane.rate_hz,
        audio_bits: audio_plane.bits,
        audio_frame_us: audio_plane.frame_us,
        // The coupling the encoder runs; a legacy answer keeps the Welcome byte-identical.
        audio_layout: audio_plane.layout.wire(),
        // Idle-keepalive re-encodes are marked `USER_FLAG_REPEAT` so client ABR treats an
        // unflagged AU as new content.
        host_caps2: punktfunk_core::quic::HOST_CAP2_REPEAT_MARK
            // Without the bit the client falls back to trackpad instead of sending contacts
            // the injector cannot land (wlroots) or cannot create a device for (Windows < 1809).
            | if crate::inject::touch_supported() {
                punktfunk_core::quic::HOST_CAP2_TOUCH
            } else {
                0
            }
            // Invites the client's `Start` extension block, which is where it names itself.
            | punktfunk_core::quic::HOST_CAP2_EXT,
    };
    io::write_msg(send, &welcome.encode()).await?;
    bringup.mark("welcome");

    // Display prep now: mode is final in Welcome; nothing in create→encoder needs Start or
    // the punched socket. The prep thread becomes the stream thread. Windows only — Linux
    // binds launch before create (gamescope nests the command), which must not run if Start
    // never arrives. A dropped channel releases the monitor into keep-alive like a normal end.
    let prep: Option<super::stream::PrepHandle> = match (source, compositor) {
        (Punktfunk1Source::Virtual, Some(comp)) if cfg!(target_os = "windows") => {
            let (ctx_tx, ctx_rx) = std::sync::mpsc::sync_channel::<SessionContext>(1);
            let client_identity = conn.peer_fingerprint();
            let client_hdr = hello.display_hdr.map(crate::encode::hdr_meta_from_wire);
            // Read back off Welcome so the prepared display and session wiring cannot disagree.
            let cursor_fw = welcome.host_caps & punktfunk_core::quic::HOST_CAP_CURSOR != 0;
            // Same bit SessionContext reads; a different max_slices would change the wire mid-flow.
            let multi_slice = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE != 0;
            let (mode, shard_payload) = (hello.mode, welcome.shard_payload);
            // Sampled here so the closure need not capture `hello`. PyroWave is always Automatic.
            // The build may re-resolve if the source delivers a different size.
            let bitrate_auto = hello.bitrate_kbps == 0 || codec == crate::encode::Codec::PyroWave;
            // `bitrate_kbps` is the wire budget; the prep encoder opens at the derived video
            // rate, snapshotted at Welcome's initial FEC percent. The FEC watcher re-derives.
            let enc_of = super::EncDerive {
                audio_kbps: super::audio_reserved_kbps(&welcome),
                shard_payload: welcome.shard_payload,
                fec_percent: welcome.fec.fec_percent,
                identity: codec == crate::encode::Codec::PyroWave,
            };
            let trace = bringup.clone();
            std::thread::Builder::new()
                .name("punktfunk1-stream".into())
                .spawn(move || -> Result<()> {
                    let prepared = super::stream::prepare_display(
                        comp,
                        mode,
                        client_identity,
                        client_hdr,
                        cursor_fw,
                        multi_slice,
                        bitrate_kbps,
                        bitrate_auto,
                        bit_depth,
                        session_hdr,
                        // `enc_of` after the depth pair; before `bit_depth` does not compile on Windows.
                        enc_of,
                        chroma,
                        codec,
                        shard_payload,
                        &quit,
                        &stop,
                        &trace,
                    );
                    let Ok(ctx) = ctx_rx.recv() else {
                        // Handshake abort / punch failure: dropping `prepared` hands the monitor
                        // to keep-alive, like a normal session end.
                        return Ok(());
                    };
                    match prepared {
                        Ok(p) => virtual_stream(ctx, Some(p)),
                        Err(e) => Err(e),
                    }
                })
                .map(|handle| (ctx_tx, handle))
                .map_err(|e| {
                    tracing::warn!(error = %e,
                        "display-prep thread spawn failed — falling back to inline bring-up")
                })
                .ok()
        }
        _ => None,
    };

    let start_msg = io::read_msg(recv).await?;
    let start = Start::decode(&start_msg).map_err(|e| anyhow!("Start decode: {e:?}"))?;
    // What the client calls itself, when it sent one. A label for the log: a bad block fails the
    // handshake (`decode_ext`'s rule), an unknown tag is skipped, and absence says nothing.
    let client_label = Start::decode_ext(&start_msg)
        .map_err(|e| anyhow!("Start extensions: {e:?}"))?
        .into_iter()
        .find(|(tag, _)| *tag == punktfunk_core::quic::EXT_TAG_CLIENT)
        .map(|(_, v)| punktfunk_core::quic::client_label(&String::from_utf8_lossy(v)))
        .filter(|s| !s.is_empty());
    bringup.mark("start");
    // `wire_mtu::spawn_watch` is started by `serve_session` once the control-task channels
    // exist; it also drives mid-session shard renegotiation (needs the control writer).
    Ok::<_, anyhow::Error>((
        hello,
        welcome,
        udp_port,
        data_sock,
        start,
        client_label,
        compositor,
        gamescope_route,
        prep,
        joined,
    ))
}

/// Compositor for Welcome plus the gamescope route as a value; synthetic has neither.
async fn negotiate_compositor(
    source: Punktfunk1Source,
    hello: &Hello,
    client: Option<[u8; 32]>,
) -> Result<(
    Option<crate::vdisplay::Compositor>,
    Option<crate::vdisplay::GamescopeRoute>,
)> {
    // Resolve now so Welcome reports the backend we will drive. Synthetic has no compositor.
    // Blocking probes → spawn_blocking.
    let compositor = match source {
        Punktfunk1Source::Virtual => {
            let pref = hello.compositor;
            // Dedicated gamescope only if the launch id resolves to a command; an unknown id
            // must not spawn a blank "sleep infinity" gamescope. `launch_is_resolvable`, not
            // `resolve_launch`: a plugin command is loopback I/O and this is the async path.
            let has_resolvable_launch = hello
                .launch
                .as_deref()
                .is_some_and(crate::library::launch_is_resolvable);
            let dedicated =
                crate::vdisplay::wants_dedicated_game_session(has_resolvable_launch, client);
            Some(
                tokio::task::spawn_blocking(move || resolve_compositor(pref, dedicated))
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

/// Bit depth, HDR verdict, and chroma, each gated by host, client, codec, and GPU probes.
async fn negotiate_video_format(
    hello: &Hello,
    codec: crate::encode::Codec,
    compositor: Option<crate::vdisplay::Compositor>,
) -> Result<(u8, bool, crate::encode::ChromaFormat)> {
    // 10-bit only when host, client, codec, capture, and GPU all allow it. Resolved before
    // Welcome so `color` matches the stream (a can't-10-bit GPU yields 8-bit SDR).
    let host_wants_10bit = pf_host_config::config().ten_bit;
    let client_supports_10bit = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_10BIT != 0;
    // `VIDEO_CAP_HDR` is BT.2020 PQ. `VIDEO_CAP_10BIT` alone is 10-bit SDR (Main10, display untouched).
    let client_wants_hdr = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_HDR != 0;
    // Source-aware: Linux HDR depends on the compositor just resolved. Gamescope folds in
    // `hdr_capture_failed(VirtualOutput)`; GameStream's rtsp.rs check has no twin here because
    // that latch is per-source and this gate already used this session's source.
    let capture_supports_hdr = crate::capture::capturer_supports_hdr_for(compositor);
    // SDR-10 needs a backend that writes 10 bits from an SDR desktop's 8-bit capture:
    // direct-NVENC (`backend_carries_sdr10`). A Linux 4:4:4 session is clamped to 8-bit
    // separately at the resolved-chroma gate below, so depth needs no chroma input here.
    let sdr10_chain_ok = codec_carries_sdr10(codec) && crate::encode::backend_carries_sdr10(codec);
    let depth_reachable = (client_wants_hdr && capture_supports_hdr) || sdr10_chain_ok;
    // Probe may open a tiny encoder; spawn_blocking, short-circuited behind the cheap gates.
    let gpu_can_10bit =
        if host_wants_10bit && client_supports_10bit && codec.supports_10bit() && depth_reachable {
            tokio::task::spawn_blocking(move || crate::encode::can_encode_10bit(codec))
                .await
                .context("10-bit capability probe task")?
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
        sdr10_chain_ok,
        codec = ?codec,
        gpu_can_10bit,
        client_video_caps = hello.video_caps,
        "encode bit depth"
    );

    // 4:4:4 only when host, client, ingest chain, and GPU all allow it. Resolved before
    // Welcome so the client sizes its decoder from what we will actually emit.
    let host_wants_444 = pf_host_config::config().four_four_four;
    let client_supports_444 = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_444 != 0;
    // Ingest chain, not capturer: Windows 4:4:4 needs direct-NVENC RGB ingest (AMF cannot;
    // QSV/ffmpeg has no RGB 4:4:4 wiring). PyroWave does its own RGB→YCbCr; its gate is
    // `can_encode_444`. HDR does not cost chroma (HEVC Main 4:4:4 10).
    let ingest_chain_supports_444 = codec == crate::encode::Codec::PyroWave
        || crate::capture::capturer_supports_444(crate::encode::resolved_backend_ingests_rgb_444());
    // Probe opens a tiny encoder; spawn_blocking, short-circuited. A negative latches until
    // restart — a GPU either supports HEVC 4:4:4 or it does not.
    let gpu_supports_444 = if matches!(
        codec,
        crate::encode::Codec::H265 | crate::encode::Codec::PyroWave
    ) && host_wants_444
        && client_supports_444
        && ingest_chain_supports_444
    {
        tokio::task::spawn_blocking(move || crate::encode::can_encode_444(codec))
            .await
            .context("4:4:4 capability probe task")?
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
        && !crate::encode::pyrowave_mode_fits_rdo(hello.mode.width, hello.mode.height, true)
    {
        tracing::warn!(
            mode = %format_args!("{}x{}", hello.mode.width, hello.mode.height),
            "PyroWave 4:4:4 at this mode exceeds the rate controller's block-index range — \
             negotiating 4:2:0"
        );
        crate::encode::ChromaFormat::Yuv420
    } else {
        chroma
    };
    #[cfg(target_os = "linux")]
    let chroma = linux_chroma_under_hdr(chroma, session_hdr);
    tracing::info!(
        chroma = ?chroma,
        host_wants_444,
        client_supports_444,
        ingest_chain_supports_444,
        "encode chroma"
    );

    // Linux 4:4:4 is CPU swscale → 8-bit `YUV444P`; a 10-bit SDR session would silently encode
    // 8-bit (HDR already took 4:2:0 above). Clamp depth before Welcome. Windows NVENC keeps 10.
    #[cfg(target_os = "linux")]
    let bit_depth: u8 = if chroma.is_444() && bit_depth == 10 {
        tracing::info!("4:4:4 on the Linux path encodes 8-bit YUV444P — resolving bit depth 8");
        8
    } else {
        bit_depth
    };
    // Follows the depth clamp: an 8-bit stream is never labelled HDR.
    let session_hdr = session_hdr && bit_depth == 10;

    Ok((bit_depth, session_hdr, chroma))
}

/// Linux 4:4:4 is 8-bit, so it cannot carry HDR. A client that asked for both keeps HDR at
/// 4:2:0: a game only offers HDR on an HDR display, while 4:4:4 only sharpens text.
#[cfg(target_os = "linux")]
fn linux_chroma_under_hdr(
    chroma: crate::encode::ChromaFormat,
    session_hdr: bool,
) -> crate::encode::ChromaFormat {
    if !(chroma.is_444() && session_hdr) {
        return chroma;
    }
    tracing::info!(
        "4:4:4 and HDR both requested — the Linux 4:4:4 path is 8-bit, so this session keeps \
         HDR and negotiates 4:2:0"
    );
    crate::encode::ChromaFormat::Yuv420
}

/// Codecs that carry 10-bit SDR off the packed-RGB capture. PyroWave is out: its capture path
/// hands NV12 under SDR, so a 10-bit label would outrun the stream.
fn codec_carries_sdr10(codec: crate::encode::Codec) -> bool {
    matches!(
        codec,
        crate::encode::Codec::H265 | crate::encode::Codec::Av1
    )
}

/// Whether Hello carried a format at all. Decode maps an absent one to 48 kHz/16-bit, so
/// that pair (or a bare zero) is "nothing asked" — the rule `Hello::encode` applies.
fn audio_format_asked(rate_hz: u32, bits: u8) -> bool {
    use punktfunk_core::audio::{pcm, SAMPLE_RATE_HZ};
    (rate_hz != 0 && rate_hz != SAMPLE_RATE_HZ) || (bits != 0 && bits != pcm::BITS_16)
}

/// Audio plane for Welcome: Opus, or lossless PCM when client, operator, and the
/// capture-rate probe all allow it. Async for that blocking probe.
async fn negotiate_audio_plane(
    conn: &super::link::SessionLink,
    hello: &Hello,
    audio_channels: u8,
    bitrate_kbps: u32,
) -> Result<AudioPlane> {
    // `conn.max_datagram_size()` is quinn's current value; MTU discovery has not settled at
    // Welcome. Frame duration is a Welcome promise with no mid-session restatement, so the
    // conservative initial MTU is the safe direction: a frame that fits now keeps fitting as
    // MTU grows; a frame sized for a discovered MTU that then fails would not be sent.
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
    Ok(resolve_audio_plane(
        hires_asked,
        hires_allowed,
        hello.audio_rate_hz,
        hello.audio_bits,
        audio_channels,
        capture_rate,
        bitrate_kbps,
        conn.max_datagram_size(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::audio::pcm;

    /// HDR takes 4:2:0 over Linux's 8-bit 4:4:4; without HDR the chroma stands.
    #[cfg(target_os = "linux")]
    #[test]
    fn hdr_outranks_444_on_linux() {
        use crate::encode::ChromaFormat::{Yuv420, Yuv444};
        assert_eq!(linux_chroma_under_hdr(Yuv444, true), Yuv420);
        assert_eq!(linux_chroma_under_hdr(Yuv444, false), Yuv444);
        assert_eq!(linux_chroma_under_hdr(Yuv420, true), Yuv420);
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

    /// AV1 carries 10-bit SDR like HEVC; PyroWave captures NV12 under SDR, so it must not.
    #[test]
    fn av1_carries_sdr10_and_pyrowave_does_not() {
        use crate::encode::Codec;
        assert!(codec_carries_sdr10(Codec::Av1));
        assert!(codec_carries_sdr10(Codec::H265));
        assert!(!codec_carries_sdr10(Codec::PyroWave));
        assert!(!codec_carries_sdr10(Codec::H264));
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
