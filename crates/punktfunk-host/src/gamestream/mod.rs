//! GameStream control plane: mDNS, nvhttp (serverinfo + pairing), RTSP, and the
//! ENet control stream. `tokio`/`axum` live here; the per-frame path is
//! `stream`/`video`/`audio` on their own threads.
//!
//! Evidence: `design/gamestream-host-plan.md`.

// Moonlight modules and `rusty_enet`/`rsa` exist only with `feature = "gamestream"`.
// `AppState`, `Host`, ports, pairing persistence, `serve`, and `tls` stay in every build.
#[cfg(feature = "gamestream")]
pub mod apps;
// Non-Linux builds get a stub `start` inside this module.
#[cfg(feature = "gamestream")]
mod audio;
#[cfg(feature = "gamestream")]
pub(crate) mod cert;
#[cfg(feature = "gamestream")]
mod control;
#[cfg(feature = "gamestream")]
mod crypto;
#[cfg(feature = "gamestream")]
pub mod gamepad;
#[cfg(feature = "gamestream")]
mod input;
#[cfg(feature = "gamestream")]
mod mdns;
#[cfg(feature = "gamestream")]
mod nvhttp;
#[cfg(feature = "gamestream")]
pub mod pairing;
/// Moonlight `SS_PEN`/`SS_TOUCH` → native pen / wire touch. See `design/pen-tablet-input.md`.
#[cfg(feature = "gamestream")]
mod pen;
#[cfg(feature = "gamestream")]
mod rtsp;
#[cfg(feature = "gamestream")]
mod serverinfo;
#[cfg(feature = "gamestream")]
mod stream;
pub(crate) mod tls;
#[cfg(feature = "gamestream")]
mod video;

use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::Arc;

/// nvhttp ports. Moonlight derives every stream port as an offset from HTTP 47989.
pub const HTTP_PORT: u16 = 47989;
pub const HTTPS_PORT: u16 = 47984;
pub const RTSP_PORT: u16 = 48010;
pub const VIDEO_PORT: u16 = 47998;
pub const CONTROL_PORT: u16 = 47999;
pub const AUDIO_PORT: u16 = 48000;

/// Per-session A/V ping payload. SETUP hex-encodes these 8 bytes as 16 characters the client echoes.
#[cfg(feature = "gamestream")]
pub const AV_PING_LEN: usize = 8;

/// Grace after an unverified owner datagram, before adopting it as the media endpoint.
#[cfg(feature = "gamestream")]
const AV_PING_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Hard wait for any owner datagram on a media port.
#[cfg(feature = "gamestream")]
const AV_PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// True when `datagram` starts with this session's ping: SETUP ASCII hex or the raw 8 bytes.
/// Trailing bytes are allowed (`SS_PING` appends a sequence). Compared constant-time.
#[cfg(feature = "gamestream")]
fn ping_matches(datagram: &[u8], expect: &[u8; AV_PING_LEN]) -> bool {
    let hex = hex::encode(expect);
    let ascii =
        datagram.len() >= hex.len() && crypto::ct_eq(&datagram[..hex.len()], hex.as_bytes());
    let raw = datagram.len() >= expect.len() && crypto::ct_eq(&datagram[..expect.len()], expect);
    ascii || raw
}

/// Learn a media stream's client UDP endpoint from the first datagram that belongs to this session.
///
/// Source IP is a filter, not a proof: only the launch owner's packets are considered, but a
/// NAT neighbour or spoofed peer can share that address. The per-session ping minted at
/// `/launch` is the proof; a racer who never saw it cannot produce it.
///
/// The payload check prefers rather than requires. The wire reference does not pin the
/// encoding, so a hard gate would black-screen a correct client. An unverified owner
/// datagram is held and adopted only if the grace window expires with nothing better.
#[cfg(feature = "gamestream")]
pub fn learn_client_endpoint(
    sock: &UdpSocket,
    label: &str,
    owner_ip: Option<IpAddr>,
    expect: &[u8; AV_PING_LEN],
) -> Result<std::net::SocketAddr> {
    let start = std::time::Instant::now();
    let deadline = start + AV_PING_TIMEOUT;
    let mut probe = [0u8; 256];
    // First unverified owner datagram, copied because `probe` is overwritten by later reads.
    let mut fallback: Option<(std::net::SocketAddr, Vec<u8>)> = None;
    let mut grace = deadline;
    loop {
        let remaining = grace.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        sock.set_read_timeout(Some(remaining))?;
        // Timeout here is the grace window, not a failure, once a fallback is in hand.
        let Ok((n, src)) = sock.recv_from(&mut probe) else {
            break;
        };
        if owner_ip.is_some_and(|ip| ip != src.ip()) {
            continue;
        }
        if ping_matches(&probe[..n], expect) {
            tracing::info!(%src, "{label}: client endpoint learned (ping payload verified)");
            return Ok(src);
        }
        if fallback.is_none() {
            fallback = Some((src, probe[..n.min(32)].to_vec()));
            grace = (std::time::Instant::now() + AV_PING_GRACE).min(deadline);
        }
    }
    match fallback {
        Some((src, head)) => {
            tracing::warn!(
                %src,
                bytes = %hex::encode(&head),
                "{label}: first datagram did not carry this session's ping payload — adopting it anyway (source-IP-bound only); report these bytes, they pin the wire encoding"
            );
            Ok(src)
        }
        None => anyhow::bail!("{label}: no client ping from the launch owner within 10s"),
    }
}

/// Advertised host version. Major ≥ 7 tells Moonlight to use SHA-256 for pairing.
pub const APP_VERSION: &str = "7.1.431.-1";
pub const GFE_VERSION: &str = "3.23.0.74";
/// `ServerCodecModeSupport` bits from moonlight-common-c `src/Limelight.h`:
/// SCM_H264 0x1, SCM_HEVC 0x100, SCM_HEVC_MAIN10 0x200, SCM_AV1_MAIN8 0x10000, SCM_AV1_MAIN10 0x20000.
pub const SCM_H264: u32 = 0x0000_0001;
pub const SCM_HEVC: u32 = 0x0000_0100;
pub const SCM_HEVC_MAIN10: u32 = 0x0000_0200;
pub const SCM_AV1_MAIN8: u32 = 0x0001_0000;
pub const SCM_AV1_MAIN10: u32 = 0x0002_0000;
/// SDR baseline: H.264 + HEVC Main + AV1 Main 8-bit. HEVC Main10 is layered at runtime by
/// `serverinfo::codec_mode_support` only when [`host_hdr_capable`] is true — a non-HDR host
/// must not advertise a mode it cannot produce. 4:4:4 stays off; stock Moonlight is 4:2:0.
pub const SERVER_CODEC_MODE_SUPPORT: u32 = SCM_H264 | SCM_HEVC | SCM_AV1_MAIN8;

/// Whether this host can deliver an HDR (10-bit BT.2020 PQ) GameStream.
///
/// Gates `IsHdrSupported`, the 10-bit codec bits in serverinfo, and (with the live
/// capture check at RTSP) honoring `dynamicRangeMode`. Behind `PUNKTFUNK_10BIT`
/// (default on; `=0`/`false`/`off`/`no` disables).
///
/// Windows: always true once the policy is on — the IDD capturer can enable PQ on the
/// virtual display. Linux: portal sessions claim yes (HDR is a live monitor fact, rechecked
/// at RTSP via [`pf_capture::gnome_hdr_monitor_active`]); virtual output is HDR only when
/// [`crate::capture::capturer_supports_hdr_for`] says so (gamescope). Both Linux arms also
/// need [`crate::encode::can_encode_10bit`].
pub fn host_hdr_capable() -> bool {
    if !pf_host_config::config().ten_bit {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        true
    }
    #[cfg(target_os = "linux")]
    {
        let source_can_hdr = match pf_host_config::config().video_source.as_deref() {
            Some("portal") => true,
            // Only a gamescope virtual output can be HDR. `detect()` is the same compositor
            // the session will pick, and it is cached downstream.
            _ => crate::vdisplay::detect()
                .ok()
                .is_some_and(|c| crate::capture::capturer_supports_hdr_for(Some(c))),
        };
        // Any 10-bit encoder makes the host HDR-capable. Which bits get advertised is
        // `serverinfo::apply_hdr`; whether this session can carry it is the RTSP honor.
        source_can_hdr
            && (crate::encode::can_encode_10bit(crate::encode::Codec::H265)
                || crate::encode::can_encode_10bit(crate::encode::Codec::Av1))
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        false
    }
}

/// See [`AppState::video_hdr`].
pub type VideoHdr = std::sync::Arc<std::sync::Mutex<Option<pf_frame::HdrMeta>>>;

/// Cumulative client-loss telemetry from the control stream's periodic `0x0201` loss-stats.
/// Control thread adds; video thread's 1 Hz step reads deltas — no lock, no reset.
#[derive(Default)]
pub struct GsLossStats {
    pub lost: std::sync::atomic::AtomicU64,
    /// A report with `lost == 0` is a healthy heartbeat.
    pub reports: std::sync::atomic::AtomicU64,
}

pub struct Host {
    pub hostname: String,
    /// Persisted per-host id. Echoed in serverinfo and matched on pairing.
    pub uniqueid: String,
    pub http_port: u16,
    pub https_port: u16,
    /// `windows` | `macos` | `linux[/<family>][/<id>]` — mDNS `os=` and `HostInfo.os`.
    pub os_chain: String,
    /// os-release `PRETTY_NAME`. Surfaced as `HostInfo.os_name` only.
    pub os_name: String,
}

impl Host {
    pub fn detect() -> Result<Host> {
        let os = crate::osinfo::detect();
        Ok(Host {
            hostname: hostname_string(),
            uniqueid: load_or_create_uniqueid()?,
            http_port: HTTP_PORT,
            https_port: HTTPS_PORT,
            os_chain: os.chain.clone(),
            os_name: os.pretty.clone(),
        })
    }

    /// Best-effort primary LAN IP, re-read every call — not a field.
    ///
    /// [`Host::detect`] runs at process start, often before DHCP. A snapshot taken then
    /// would advertise `127.0.0.1` for the life of the process. A `connect(2)` on an
    /// unconnected UDP socket sends no packets. Loopback here means "still no LAN address".
    pub fn local_ip(&self) -> IpAddr {
        primary_local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
    }
}

/// Client `/launch` parameters, shared with RTSP and the media stages.
#[derive(Clone, Copy, Debug)]
pub struct LaunchSession {
    /// AES-128 key for RTSP/control/video/audio (`rikey`).
    pub gcm_key: [u8; 16],
    /// Seeds the per-stream GCM IVs (`rikeyid`).
    pub rikeyid: i32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `/launch?appid=N` — app-catalog entry for this session.
    pub appid: u32,
    /// Source IP of the paired HTTPS client that issued `/launch`. Unauthenticated
    /// RTSP/UDP binds to this so an unpaired peer cannot ride the launch. `None` if
    /// the address could not be captured (RTSP then falls back to launch-present only).
    pub peer_ip: Option<std::net::IpAddr>,
    /// SHA-256 cert fingerprint of the paired client that owns this session. Mode-conflict
    /// admission compares it to tell a same-client re-launch (always allowed) from a different
    /// client (subject to `mode_conflict`). `[u8; 32]` keeps [`LaunchSession`] `Copy`; `None`
    /// when the peer cert could not be read.
    pub owner_fp: Option<[u8; 32]>,
}

pub struct AppState {
    pub host: Host,
    /// GameStream RSA-2048 identity. Moonlight pins it; pairing hashes bind its X.509 bytes.
    /// Native planes present `crate::identity` instead.
    #[cfg(feature = "gamestream")]
    pub identity: cert::ServerIdentity,
    #[cfg(feature = "gamestream")]
    pub pairing: pairing::Pairing,
    /// Paired client certificate DERs. Unconditional so a native-only build can still list
    /// and revoke pairings made by a GameStream-featured build sharing the config dir.
    pub paired: std::sync::Mutex<Vec<Vec<u8>>>,
    /// Bound only while `paired` is non-empty. See [`control::sync`] / [`sync_control`].
    #[cfg(feature = "gamestream")]
    pub(crate) control_gate: control::Gate,
    /// Set by `/launch`, consumed by RTSP/media.
    pub launch: std::sync::Mutex<Option<LaunchSession>>,
    /// This session's A/V ping payload, minted by `/launch` and `/resume`. Not in
    /// [`LaunchSession`]: the client does not supply it, and resume re-mints while that
    /// struct's keys may not. See [`learn_client_endpoint`].
    #[cfg(feature = "gamestream")]
    pub av_ping: std::sync::atomic::AtomicU64,
    /// RTSP ANNOUNCE video config, consumed on PLAY.
    #[cfg(feature = "gamestream")]
    pub stream: std::sync::Mutex<Option<stream::StreamConfig>>,
    /// RTSP ANNOUNCE audio parameters. Defaults to stereo when the client never ANNOUNCEs them.
    #[cfg(feature = "gamestream")]
    pub audio_params: std::sync::Mutex<audio::AudioParams>,
    /// Video thread running, and its keep-running flag.
    pub streaming: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Deliberate end (compat-plane stand-in for native `QUIT_CODE`; RTSP has none).
    ///
    /// Set by `/cancel`, management stop, and the launched game exiting. An ENet vanish
    /// leaves it clear. Virtual-display linger and end-game policy both read it. Cleared
    /// by `/launch`.
    pub quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The display's admission stop flag: raised when another client steals it.
    /// Read by [`AppState::end_if_preempted`].
    #[cfg(feature = "gamestream")]
    pub preempted: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Audio thread running, and its keep-running flag.
    pub audio_streaming: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Bumped by each media thread as the last thing it does on exit, after teardown.
    /// `/resume` waits on this so the old capturer-pool and lease teardown finish before
    /// the successor starts.
    pub media_exited: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Client IDR / reference-frame invalidation request. Video thread forces a keyframe and clears it.
    pub force_idr: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Client 0x0301 lost-frame range. Video thread drains it into `Encoder::invalidate_ref_frames`,
    /// falling back to a full IDR when the encoder cannot invalidate. `None` = nothing pending.
    pub rfi_range: std::sync::Arc<std::sync::Mutex<Option<(i64, i64)>>>,
    /// Cumulative `0x0201` loss-stats from [`control`]. Video thread's 1 Hz step reads window deltas.
    pub loss_stats: std::sync::Arc<GsLossStats>,
    /// Mastering metadata of the frames the video thread encodes, `None` while they are SDR.
    /// [`control`] tells the client each change (`0x010e`).
    #[cfg(feature = "gamestream")]
    pub video_hdr: VideoHdr,
    /// This session's input tallies, bumped by [`control`] and read by the summary. One
    /// session at a time on this plane, so the video thread clears them at stream start —
    /// without that, `session.ended` would report zeros nobody counted.
    pub counters: Arc<crate::session_status::SessionCounters>,
    /// Persistent screen capturer, reused across streams. The slot's `bool` is whether it was
    /// opened with the HDR offer; a stream whose negotiated `hdr` differs drops it and opens
    /// a fresh session at the right depth.
    #[cfg(feature = "gamestream")]
    pub video_cap: stream::CapturerSlot,
    /// Persistent audio capturer. Reused when channel count matches (drained so no stale
    /// audio is sent); dropped and reopened when a session negotiates a different count.
    pub audio_cap: std::sync::Arc<std::sync::Mutex<Option<Box<dyn crate::audio::AudioCapturer>>>>,
    /// Shared streaming-stats recorder. The same `Arc` is handed to mgmt, GameStream, and
    /// native loops so one capture spans whichever path is streaming.
    pub stats: Arc<crate::stats_recorder::StatsRecorder>,
    /// Per-client access grants, keyed by certificate fingerprint hex. Same registry as the
    /// native trust store. Set once by [`serve`]; if unset, every paired peer is ungoverned.
    pub access: std::sync::OnceLock<Arc<crate::native_pairing::NativePairing>>,
}

/// Callback media threads invoke on a UDP send error: ends the whole session via
/// [`AppState::end_session`], not just the noticing thread. Built by the RTSP PLAY handler.
pub(crate) type OnSessionLost = Arc<dyn Fn() + Send + Sync>;

impl AppState {
    /// Stop both media threads and clear launch + negotiated stream config. Idempotent.
    ///
    /// Anything less leaves a stale session: a lingering `launch` 503-blocks another
    /// client's `/launch` under `mode_conflict = reject`, and `streaming = true` makes a
    /// reconnect's PLAY take the "already running" branch while old threads still stream
    /// at the vanished endpoint. Returns whether video was live.
    pub(crate) fn end_session(&self, reason: &str) -> bool {
        use std::sync::atomic::Ordering;
        let was_streaming = self.streaming.swap(false, Ordering::SeqCst);
        let was_audio = self.audio_streaming.swap(false, Ordering::SeqCst);
        let had_launch = self
            .launch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_some();
        #[cfg(feature = "gamestream")]
        self.stream.lock().unwrap_or_else(|e| e.into_inner()).take();
        if was_streaming || was_audio || had_launch {
            tracing::info!(
                reason,
                was_streaming,
                was_audio,
                had_launch,
                "gamestream: session ended"
            );
        }
        was_streaming
    }

    /// Mark the end deliberate, then tear down. Used by `/cancel`, management stop, and
    /// game exit. See [`AppState::quit`].
    pub(crate) fn quit_session(&self, reason: &str) -> bool {
        self.quit.store(true, std::sync::atomic::Ordering::SeqCst);
        self.end_session(reason)
    }

    /// End the session if another client stole its display since the last call. A steal is
    /// a drop, not a quit: the display lingers for the stealer, as a native victim's does.
    #[cfg(feature = "gamestream")]
    pub(crate) fn end_if_preempted(&self) -> bool {
        if !self
            .preempted
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        self.end_session("another client took the display");
        true
    }

    /// Mint a fresh A/V ping for `/launch` or `/resume`. Must run before the client's RTSP SETUP.
    #[cfg(feature = "gamestream")]
    pub fn mint_av_ping(&self) -> [u8; AV_PING_LEN] {
        let payload = crypto::random::<AV_PING_LEN>();
        self.av_ping.store(
            u64::from_be_bytes(payload),
            std::sync::atomic::Ordering::SeqCst,
        );
        payload
    }

    /// This session's A/V ping — what SETUP advertises and the media planes expect back.
    #[cfg(feature = "gamestream")]
    pub fn av_ping_payload(&self) -> [u8; AV_PING_LEN] {
        self.av_ping
            .load(std::sync::atomic::Ordering::SeqCst)
            .to_be_bytes()
    }

    /// Fresh control-plane state. Pairing allow-list is loaded from disk. `stats` is the
    /// shared recorder handed to mgmt and the streaming loops.
    #[cfg(feature = "gamestream")]
    pub fn new(
        host: Host,
        identity: cert::ServerIdentity,
        stats: Arc<crate::stats_recorder::StatsRecorder>,
    ) -> AppState {
        AppState {
            host,
            identity,
            pairing: pairing::Pairing::new(),
            paired: std::sync::Mutex::new(load_paired()),
            control_gate: control::Gate::new(),
            launch: std::sync::Mutex::new(None),
            av_ping: std::sync::atomic::AtomicU64::new(0),
            stream: std::sync::Mutex::new(None),
            audio_params: std::sync::Mutex::new(audio::AudioParams::default()),
            streaming: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            quit: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            preempted: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            audio_streaming: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            force_idr: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            rfi_range: std::sync::Arc::new(std::sync::Mutex::new(None)),
            loss_stats: std::sync::Arc::new(GsLossStats::default()),
            video_hdr: VideoHdr::default(),
            counters: Arc::new(crate::session_status::SessionCounters::default()),
            media_exited: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            video_cap: std::sync::Arc::new(std::sync::Mutex::new(None)),
            audio_cap: std::sync::Arc::new(std::sync::Mutex::new(None)),
            stats,
            access: std::sync::OnceLock::new(),
        }
    }

    /// Native-only [`AppState::new`]: same control-plane state minus Moonlight identity,
    /// pairing, control-gate, and RTSP stream slots.
    #[cfg(not(feature = "gamestream"))]
    pub fn new(host: Host, stats: Arc<crate::stats_recorder::StatsRecorder>) -> AppState {
        AppState {
            host,
            paired: std::sync::Mutex::new(load_paired()),
            launch: std::sync::Mutex::new(None),
            streaming: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            quit: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            audio_streaming: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            force_idr: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            rfi_range: std::sync::Arc::new(std::sync::Mutex::new(None)),
            loss_stats: std::sync::Arc::new(GsLossStats::default()),
            counters: Arc::new(crate::session_status::SessionCounters::default()),
            media_exited: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            audio_cap: std::sync::Arc::new(std::sync::Mutex::new(None)),
            stats,
            access: std::sync::OnceLock::new(),
        }
    }
}

/// Bind the ENet control port iff at least one pairing exists. Crate-visible so mgmt
/// unpair can reach past the private `control` module. No-op unless `serve` armed the gate.
#[cfg(feature = "gamestream")]
pub(crate) fn sync_control(state: &Arc<AppState>) -> Result<()> {
    control::sync(state)
}

/// Native-only: no ENet port. Callers (mgmt unpair) stay uniform.
#[cfg(not(feature = "gamestream"))]
pub(crate) fn sync_control(_state: &Arc<AppState>) -> Result<()> {
    Ok(())
}

/// Run the host (blocks).
///
/// Native punktfunk/1 (QUIC on `native.port`) and the management API always run and
/// share one [`crate::native_pairing`] handle. `gamestream` additionally brings up
/// nvhttp pairing, RTSP, ENet control, and `_nvstream` mDNS. Those planes pair over
/// plain HTTP and can reuse GCM nonces, so they are opt-in (`serve --gamestream`)
/// and for a trusted LAN only.
pub fn serve(
    mgmt: crate::mgmt::Options,
    native: crate::native::NativeServe,
    gamestream: bool,
) -> Result<()> {
    // `serve --gamestream` against a native-only binary is an explicit ask this build
    // cannot honor — refuse rather than quietly serve less than configured.
    #[cfg(not(feature = "gamestream"))]
    if gamestream {
        anyhow::bail!(
            "this punktfunk-host was built WITHOUT the 'gamestream' feature — stock-Moonlight \
             compat is unavailable in this binary. Remove --gamestream / PUNKTFUNK_GAMESTREAM \
             from the configuration, or install a standard (GameStream-featured) build."
        );
    }
    let host = Host::detect()?;
    let stats = crate::stats_recorder::StatsRecorder::new(crate::stats_recorder::default_dir());
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(None, None, false)
            .context("native pairing store")?,
    );
    // Native identity first. If GameStream writes `cert.pem` before a native pair
    // exists, the console starts and serves the SAN-less RSA cert. Reading the dir
    // first also stops `load_or_create` minting a new cert that `load_or_adopt`
    // would then treat as the pin it was preserving.
    let native_ident = crate::identity::load_or_adopt(&np).context("native host identity")?;
    #[cfg(feature = "gamestream")]
    let state = {
        let identity = cert::ServerIdentity::load_or_create().context("host certificate")?;
        Arc::new(AppState::new(host, identity, stats.clone()))
    };
    #[cfg(not(feature = "gamestream"))]
    let state = Arc::new(AppState::new(host, stats.clone()));
    // Hand GameStream the grants registry so nvhttp launch and ENet resolve a Moonlight
    // fingerprint against the same mask the native plane enforces.
    let _ = state.access.set(np.clone());
    tracing::info!(
        hostname = %state.host.hostname,
        uniqueid = %state.host.uniqueid,
        ip = %state.host.local_ip(),
        native_port = native.port,
        require_pairing = native.require_pairing,
        gamestream,
        "punktfunk host"
    );
    crate::net_health::log_addresses();
    crate::net_health::spawn_route_watch();
    // Scan once (cached for `/local/summary`). Warn only when a clash is active;
    // a dormant leftover logs at INFO so every boot is not a warning.
    let conflicts = crate::detect::init();
    if !conflicts.is_empty() {
        let report = crate::detect::render_report(conflicts);
        if crate::detect::any_active(conflicts) {
            tracing::warn!(
                target: "punktfunk::detect",
                count = conflicts.len(),
                "{report}"
            );
        } else {
            tracing::info!(
                target: "punktfunk::detect",
                count = conflicts.len(),
                "{report}"
            );
        }
    }
    if gamestream {
        tracing::warn!(
            "GameStream/Moonlight compat ENABLED (--gamestream): its pairing runs over plain HTTP and \
             its legacy control encryption can reuse GCM nonces (security-review #5/#9) — an on-path \
             LAN attacker could MITM pairing or recover input. Enable only on a TRUSTED network; prefer \
             the native punktfunk/1 plane + clients for untrusted/WAN use."
        );
    }
    if let Some(bind) = native.webtransport_bind {
        tracing::warn!(
            %bind,
            "WebTransport browser plane ENABLED (--webtransport): a second, externally-reachable \
             transport whose certificate hash is published unauthenticated. A browser pairs over \
             PAKE with its own device key, which is what proves it — the published hash does not."
        );
    }
    let rt = tokio::runtime::Runtime::new().context("build tokio runtime")?;
    rt.block_on(async move {
        // rustls needs a process-wide crypto provider before any TLS config is built.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let native_opts = crate::native::native_serve_opts(&native);
        // Hook runner consumes the live event tail for the host's lifetime. Spawned
        // before `host.started` so operator hooks observe the full lifecycle.
        tokio::spawn(crate::hooks::runner());
        // The browser plane, when the operator asked for it. The native plane spawns it, because
        // a browser runs the native session on the native plane's state. The long-lived identity
        // signs the plane's throwaway certificate, so a paired browser can check it against the
        // fingerprint it pinned. Same pairing store and same flag as the native plane: a device
        // is paired with the host, not with a plane.
        let web = native
            .webtransport_bind
            .map(|bind| crate::webtransport::Plane {
                bind,
                sans: vec![
                    state.host.local_ip().to_string(),
                    "localhost".to_string(),
                    "127.0.0.1".to_string(),
                ],
                origins: pf_host_config::config().webtransport_origins.clone(),
                identity: native_ident.clone(),
                pairing: np.clone(),
                require_pairing: native.require_pairing,
            });
        // Read before `web` is moved into the native plane below. The management API answers a
        // cross-origin call only where there is a browser to answer.
        let browser_plane = web.is_some();
        // `host.started` as the planes come up; `host.stopping` on clean or error exit
        // so a consumer that reconnects still sees it.
        crate::events::emit(crate::events::EventKind::HostStarted {
            version: env!("CARGO_PKG_VERSION").to_string(),
            gamestream,
        });
        let served: anyhow::Result<()> = if gamestream {
            // `gamestream` is only true when the feature is compiled in; serve() bails otherwise.
            #[cfg(not(feature = "gamestream"))]
            {
                unreachable!("serve() refuses --gamestream in a native-only build")
            }
            #[cfg(feature = "gamestream")]
            {
                // `_nvstream` advert is fatal on failure: Moonlight cannot find the host
                // without it. `--no-mdns` / PUNKTFUNK_MDNS=0 skips it when multicast is dead.
                let _advert = if native.mdns {
                    Some(mdns::advertise(&state.host).context("mDNS advertise")?)
                } else {
                    tracing::info!(
                        "GameStream mDNS advertisement disabled (--no-mdns / PUNKTFUNK_MDNS)"
                    );
                    None
                };
                rtsp::spawn(state.clone()).context("start RTSP server")?;
                // ENet (`rusty_enet`, transpiled C) binds only while a pairing exists.
                // Pairing is HTTPS on nvhttp and never touches 47999; the port re-syncs
                // when the first client pins, before that client can `/launch`.
                state.control_gate.enable();
                sync_control(&state).context("start ENet control server")?;
                tracing::info!(
                    port = native.port,
                    "unified host: GameStream/Moonlight compat + native punktfunk/1 (QUIC)"
                );
                tokio::try_join!(
                    nvhttp::run(state.clone()),
                    crate::mgmt::run(
                        state.clone(),
                        mgmt,
                        Some(np.clone()),
                        stats.clone(),
                        gamestream,
                        native_ident.clone(),
                        browser_plane,
                    ),
                    crate::native::serve(
                        native_opts,
                        native.mgmt_port,
                        np,
                        stats.clone(),
                        native_ident,
                        web,
                    ),
                )
                .map(|_| ())
            }
        } else {
            tracing::info!(
                port = native.port,
                "secure host: native punktfunk/1 (QUIC) + management API \
                 (GameStream OFF — pass --gamestream for stock-Moonlight compat)"
            );
            tokio::try_join!(
                crate::mgmt::run(
                    state.clone(),
                    mgmt,
                    Some(np.clone()),
                    stats.clone(),
                    gamestream,
                    native_ident.clone(),
                    browser_plane,
                ),
                crate::native::serve(
                    native_opts,
                    native.mgmt_port,
                    np,
                    stats.clone(),
                    native_ident,
                    web,
                ),
            )
            .map(|_| ())
        };
        crate::events::emit(crate::events::EventKind::HostStopping);
        served
    })
}

/// Host wall clock, unix seconds. Access deadlines are stored and evaluated in this
/// clock so an NTP step moves them. Shared by nvhttp launch gates and the control
/// thread's expiry check.
#[cfg(feature = "gamestream")]
pub(crate) fn wall_unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Display name for Moonlight's host tile and both mDNS instance names.
/// `PUNKTFUNK_HOST_NAME` wins; otherwise the machine hostname.
fn hostname_string() -> String {
    if let Some(n) = pf_host_config::config().host_name.as_deref() {
        return sanitize_display_name(n);
    }
    machine_hostname()
}

/// Raw machine hostname — no `PUNKTFUNK_HOST_NAME`, no display sanitizing.
/// Certificate SAN and DNS-ish consumers want this, not [`hostname_string`].
pub(crate) fn machine_hostname() -> String {
    #[cfg(target_os = "windows")]
    if let Some(n) = std::env::var_os("COMPUTERNAME") {
        let s = n.to_string_lossy().trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "punktfunk-host".to_string())
}

/// Make an operator-supplied name safe as an mDNS service instance. `.` splits the
/// instance label (clients take the first label of the fullname), and DNS-SD caps a
/// label at 63 bytes. Control characters go too.
fn sanitize_display_name(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == '.' { '-' } else { c })
        .collect();
    // Truncate on a char boundary so a multi-byte name cannot yield invalid UTF-8.
    let mut out = String::new();
    for c in cleaned.trim().chars() {
        if out.len() + c.len_utf8() > 63 {
            break;
        }
        out.push(c);
    }
    let out = out.trim().to_string();
    if out.is_empty() {
        "punktfunk-host".to_string()
    } else {
        out
    }
}

/// Load the persisted host uniqueid, or mint from `/proc/sys/kernel/random/uuid` and store it.
fn load_or_create_uniqueid() -> Result<String> {
    let path = pf_paths::config_dir().join("uniqueid");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let t = s.trim();
        if !t.is_empty() {
            return Ok(t.to_string());
        }
    }
    let id = std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|u| u.trim().replace('-', ""))
        .unwrap_or_else(|_| format!("{:016x}{:016x}", std::process::id(), HTTP_PORT));
    std::fs::create_dir_all(pf_paths::config_dir()).ok();
    std::fs::write(&path, &id).with_context(|| format!("write {}", path.display()))?;
    Ok(id)
}

/// Best-effort primary LAN IP: a UDP `connect` toward a public address, then read the
/// local address the OS would route through. No packets are sent.
///
/// Returns `None` — never loopback — when the machine has no LAN address yet. The
/// route probe fails on a cold boot before DHCP; then the first non-loopback
/// interface address is used, which the NIC has as soon as it is configured.
pub(crate) fn primary_local_ip() -> Option<IpAddr> {
    let routed = UdpSocket::bind("0.0.0.0:0")
        .and_then(|sock| {
            sock.connect("8.8.8.8:80")?;
            sock.local_addr()
        })
        .ok()
        .map(|a| a.ip())
        .filter(|ip| usable_lan_ip(*ip));
    routed.or_else(first_lan_ipv4)
}

/// First reachable IPv4 an interface holds, ignoring the routing table.
///
/// The route probe needs a default route, which lands after the NIC has its address.
/// Between those moments this is the only answer that is not loopback.
fn first_lan_ipv4() -> Option<IpAddr> {
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .map(|i| i.ip())
        .find(|ip| ip.is_ipv4() && usable_lan_ip(*ip))
}

/// Loopback and unspecified are "we don't know yet"; advertising either publishes
/// the host as `127.0.0.1` until restart.
fn usable_lan_ip(ip: IpAddr) -> bool {
    !ip.is_loopback() && !ip.is_unspecified()
}

/// Where the paired-client allow-list persists across restarts.
fn paired_path() -> Option<std::path::PathBuf> {
    Some(pf_paths::config_dir().join("paired.json"))
}

/// Load persisted paired-client certificate DERs. Empty on first run, parse failure, or a
/// store a non-admin planted before the first elevated run.
fn load_paired() -> Vec<Vec<u8>> {
    let Some(path) = paired_path() else {
        return Vec::new();
    };
    if crate::planted::quarantine_planted_secret(&path) {
        return Vec::new();
    }
    let Ok(raw) = std::fs::read(&path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<Vec<Vec<u8>>>(&raw) {
        Ok(v) => {
            tracing::info!(clients = v.len(), "loaded persisted pairings");
            v
        }
        Err(e) => {
            tracing::warn!(error = %e, "paired.json unreadable — starting unpaired");
            Vec::new()
        }
    }
}

/// Persist the paired-client allow-list after each successful pairing. Atomic temp-file
/// + rename so a crash mid-write cannot truncate `paired.json` and lock out every client.
pub(crate) fn save_paired(paired: &[Vec<u8>]) {
    let Some(path) = paired_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = pf_paths::create_private_dir(dir);
    }
    let bytes = match serde_json::to_vec(paired) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "serializing pairings failed");
            return;
        }
    };
    // Sibling temp file (owner-only), then rename over the target. Never write `path` in place.
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = pf_paths::write_secret_file(&tmp, &bytes) {
        tracing::warn!(error = %e, "persisting pairings failed (temp write)");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        tracing::warn!(error = %e, "persisting pairings failed (rename)");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Operator-supplied per-client display labels, keyed by certificate fingerprint.
///
/// Sidecar to [`paired_path`], not a field inside it: `paired.json` is a bare
/// `Vec<Vec<u8>>` of DERs, and a label is not part of the trust decision — a
/// corrupt labels file must never lock anyone out.
///
/// Every moonlight-common-c client self-signs as `CN=NVIDIA GameStream Client`,
/// so the certificate carries no device identity; without a label, five paired
/// devices are five identical rows.
fn labels_path() -> Option<std::path::PathBuf> {
    Some(pf_paths::config_dir().join("client-labels.json"))
}

/// Serializes the read-modify-write in [`set_client_label`]. Two concurrent renames
/// would otherwise race on a whole-file rewrite and drop one of the two names.
static LABELS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Fingerprint → label map. Empty on first run, unreadable file, or parse failure —
/// a label is cosmetic, so every failure degrades to "no names".
pub(crate) fn load_client_labels() -> std::collections::BTreeMap<String, String> {
    let Some(path) = labels_path() else {
        return Default::default();
    };
    let Ok(raw) = std::fs::read(&path) else {
        return Default::default();
    };
    serde_json::from_slice(&raw).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "client-labels.json unreadable — listing clients without names");
        Default::default()
    })
}

/// Set (`Some`) or clear (`None`) one client's label, persisted atomically.
/// Fingerprints are lowercased so a rename and a later lookup agree.
pub(crate) fn set_client_label(fp_hex: &str, label: Option<&str>) -> Option<String> {
    let _guard = LABELS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fp = fp_hex.to_ascii_lowercase();
    let mut labels = load_client_labels();
    let stored = match label {
        Some(l) => {
            let clean = crate::native_pairing::sanitize_device_name(l, &fp);
            labels.insert(fp, clean.clone());
            Some(clean)
        }
        None => {
            labels.remove(&fp);
            None
        }
    };
    save_client_labels(&labels);
    stored
}

/// Drop labels whose fingerprints are no longer paired, so the file cannot grow
/// without bound and a re-pair of the same cert starts unnamed.
pub(crate) fn retain_client_labels(still_paired: &[Vec<u8>]) {
    use sha2::{Digest, Sha256};
    let _guard = LABELS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let live: std::collections::BTreeSet<String> = still_paired
        .iter()
        .map(|der| hex::encode(Sha256::digest(der)))
        .collect();
    let mut labels = load_client_labels();
    let before = labels.len();
    labels.retain(|fp, _| live.contains(fp));
    if labels.len() != before {
        save_client_labels(&labels);
    }
}

/// Persist the label map with the same atomic temp-file + rename as [`save_paired`].
fn save_client_labels(labels: &std::collections::BTreeMap<String, String>) {
    let Some(path) = labels_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = pf_paths::create_private_dir(dir);
    }
    let bytes = match serde_json::to_vec(labels) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "serializing client labels failed");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = pf_paths::write_secret_file(&tmp, &bytes) {
        tracing::warn!(error = %e, "persisting client labels failed (temp write)");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        tracing::warn!(error = %e, "persisting client labels failed (rename)");
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod host_name_tests {
    use super::sanitize_display_name;

    /// Display name rides the mDNS service instance label; a `.` truncates it in every
    /// client list. Split from the env read: `PUNKTFUNK_HOST_NAME` is process-global
    /// and must not race the parallel suite.
    #[test]
    fn display_name_survives_free_text_but_loses_the_label_breakers() {
        assert_eq!(sanitize_display_name("Living Room PC"), "Living Room PC");
        assert_eq!(sanitize_display_name("  Wohnzimmer  "), "Wohnzimmer");
        assert_eq!(sanitize_display_name("Ben's PC v1.2"), "Ben's PC v1-2");
        assert_eq!(sanitize_display_name("Küche ☕"), "Küche ☕");
        assert_eq!(sanitize_display_name("tab\there"), "tabhere");
        // Empty instance names are not registerable.
        assert_eq!(sanitize_display_name("   "), "punktfunk-host");
        // DNS-SD label ceiling is 63 bytes; truncate on a char boundary.
        let long = sanitize_display_name(&"ü".repeat(100));
        assert!(long.len() <= 63, "{} bytes", long.len());
        assert_eq!(long, "ü".repeat(31));
    }
}

#[cfg(test)]
mod local_ip_tests {
    use super::{first_lan_ipv4, primary_local_ip, usable_lan_ip};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn loopback_and_unspecified_are_never_advertisable() {
        for unusable in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ] {
            assert!(
                !usable_lan_ip(unusable),
                "{unusable} must not be advertised"
            );
        }
        for usable in [
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 173)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1)),
        ] {
            assert!(usable_lan_ip(usable), "{usable} is reachable and must pass");
        }
    }

    #[test]
    fn probe_reports_no_address_rather_than_loopback() {
        // Either a real LAN address or none. `None` lets `Host::local_ip()` and mDNS retry
        // instead of freezing a wrong answer.
        assert!(primary_local_ip().is_none_or(usable_lan_ip));
    }

    #[test]
    fn interface_fallback_never_offers_loopback() {
        // Cold-boot branch, before the default route exists. Finding nothing is fine;
        // handing back loopback from `get_if_addrs` is not.
        assert!(first_lan_ipv4().is_none_or(usable_lan_ip));
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    fn test_state() -> AppState {
        let host = Host {
            hostname: "test-host".into(),
            uniqueid: "deadbeef".into(),
            http_port: HTTP_PORT,
            https_port: HTTPS_PORT,
            os_chain: "linux".into(),
            os_name: "Linux".into(),
        };
        let stats = crate::stats_recorder::StatsRecorder::new(std::env::temp_dir().join(format!(
            "pf-gs-endsession-{}-{:p}",
            std::process::id(),
            &0u8 as *const u8
        )));
        // Session teardown under test is feature-independent; both `new` flavors.
        #[cfg(feature = "gamestream")]
        {
            let identity = cert::ServerIdentity::ephemeral().expect("ephemeral identity");
            AppState::new(host, identity, stats)
        }
        #[cfg(not(feature = "gamestream"))]
        {
            AppState::new(host, stats)
        }
    }

    /// Mint and read must agree on byte order. A resume must not reuse the old value:
    /// the previous payload may have been seen on the plaintext wire.
    #[cfg(feature = "gamestream")]
    #[test]
    fn av_ping_mint_round_trips_and_changes() {
        let state = test_state();
        let first = state.mint_av_ping();
        assert_eq!(first, state.av_ping_payload(), "advertised != expected");
        let second = state.mint_av_ping();
        assert_ne!(first, second, "a resume must not reuse the payload");
        assert_eq!(second, state.av_ping_payload());
    }

    /// One call must clear both media flags, the launch, and the negotiated stream
    /// config, and be idempotent.
    #[test]
    fn end_session_clears_the_whole_session() {
        use std::sync::atomic::Ordering;
        let state = test_state();
        state.streaming.store(true, Ordering::SeqCst);
        state.audio_streaming.store(true, Ordering::SeqCst);
        *state.launch.lock().unwrap() = Some(LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 0,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            peer_ip: None,
            owner_fp: None,
        });
        #[cfg(feature = "gamestream")]
        {
            *state.stream.lock().unwrap() = Some(stream::StreamConfig {
                width: 1920,
                height: 1080,
                fps: 60,
                packet_size: 1024,
                bitrate_kbps: 20_000,
                codec: crate::encode::Codec::H265,
                min_fec: 0,
                hdr: false,
                slices: 1, // no-request default; hardware decoders get single-slice AUs
                encrypt_video: false,
            });
        }

        assert!(state.end_session("test"), "video was live");
        assert!(!state.streaming.load(Ordering::SeqCst));
        assert!(!state.audio_streaming.load(Ordering::SeqCst));
        assert!(state.launch.lock().unwrap().is_none());
        #[cfg(feature = "gamestream")]
        assert!(state.stream.lock().unwrap().is_none());

        // Second end (`/cancel` racing ENet Disconnect) is a no-op.
        assert!(!state.end_session("test again"));
    }

    /// Compat plane has no close code, so this flag is the only difference between a
    /// player stop and a vanished client. Forgetting it silently downgrades a stop to a drop.
    #[test]
    fn quit_marks_a_teardown_deliberate_and_a_plain_end_does_not() {
        use std::sync::atomic::Ordering;
        let state = test_state();
        assert!(
            !state.quit.load(Ordering::SeqCst),
            "a fresh session is undecided"
        );

        // A drop (ENet vanish / unreachable client) must leave it clear.
        state.streaming.store(true, Ordering::SeqCst);
        state.end_session("client unreachable");
        assert!(!state.quit.load(Ordering::SeqCst));

        state.streaming.store(true, Ordering::SeqCst);
        assert!(state.quit_session("client /cancel"), "video was live");
        assert!(state.quit.load(Ordering::SeqCst));
        assert!(!state.streaming.load(Ordering::SeqCst));
    }
}

#[cfg(all(test, feature = "gamestream"))]
mod av_ping_tests {
    use super::{ping_matches, AV_PING_LEN};

    const P: [u8; AV_PING_LEN] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77];

    /// The wire reference does not pin ASCII vs raw, and the modern form wraps the
    /// payload with a sequence number. Every shape those unknowns allow must match.
    #[test]
    fn both_encodings_match_with_or_without_a_trailing_sequence() {
        let hex = b"0011223344556677";
        let raw = &P[..];
        assert!(ping_matches(hex, &P), "ASCII hex, exactly");
        assert!(ping_matches(raw, &P), "decoded bytes, exactly");
        assert!(
            ping_matches(&[&hex[..], &[0, 0, 0, 1]].concat(), &P),
            "hex + seq"
        );
        assert!(
            ping_matches(&[raw, &[0, 0, 0, 1]].concat(), &P),
            "raw + seq"
        );
    }

    #[test]
    fn anything_else_does_not_match() {
        assert!(!ping_matches(b"", &P), "empty");
        assert!(!ping_matches(b"PING", &P), "the legacy fixed ping");
        assert!(!ping_matches(b"001122334455667", &P), "one hex char short");
        assert!(!ping_matches(&P[..7], &P), "one raw byte short");
        assert!(
            !ping_matches(b"0011223344556678", &P),
            "last hex char wrong"
        );
        let mut near = P;
        near[7] ^= 1;
        assert!(!ping_matches(&near, &P), "last raw byte wrong");
        // Former fixed ping, now that every session mints its own.
        assert!(!ping_matches(b"0011223344556677", &[0xAB; AV_PING_LEN]));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn secrets_are_written_owner_only() {
        let dir = std::env::temp_dir().join(format!("pf-secret-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        pf_paths::create_private_dir(&dir).expect("create private dir");
        let dmode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700, "config dir must be owner-only (0700)");

        let key = dir.join("key.pem");
        pf_paths::write_secret_file(&key, b"-----BEGIN PRIVATE KEY-----\n...")
            .expect("write secret");
        let fmode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600, "private key must be owner-only (0600)");

        // Overwrite must keep 0600 (truncate + reopen, not create).
        pf_paths::write_secret_file(&key, b"new contents").expect("rewrite secret");
        let fmode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
