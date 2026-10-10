//! What every plane of the host shares: the host's own facts ([`Host`]), the session state
//! the management API acts on ([`AppState`]), and [`serve`], which brings the planes up.
//! GameStream's own state rides along as [`AppState::gs`] when that plane is built in.

use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::Arc;

/// This machine as every plane advertises it.
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
            http_port: crate::gamestream::HTTP_PORT,
            https_port: crate::gamestream::HTTPS_PORT,
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

/// Host-lifetime state the planes and the management API share. The Moonlight plane's own
/// slice is [`AppState::gs`], absent from a native-only build.
#[cfg_attr(not(feature = "gamestream"), allow(dead_code, reason = "compat plane"))]
pub struct AppState {
    pub host: Host,
    /// Paired client certificate DERs. Unconditional so a native-only build can still list
    /// and revoke pairings made by a GameStream-featured build sharing the config dir.
    pub paired: std::sync::Mutex<Vec<Vec<u8>>>,
    /// Set by `/launch`, consumed by RTSP/media.
    pub launch: std::sync::Mutex<Option<crate::gamestream::LaunchSession>>,
    /// Video thread running, and its keep-running flag.
    pub streaming: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Deliberate end (compat-plane stand-in for native `QUIT_CODE`; RTSP has none).
    ///
    /// Set by `/cancel`, management stop, and the launched game exiting. An ENet vanish
    /// leaves it clear. Virtual-display linger and end-game policy both read it. Cleared
    /// by `/launch`.
    pub quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Audio thread running, and its keep-running flag.
    pub audio_streaming: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Bumped by each media thread as the last thing it does on exit, after teardown.
    /// `/resume` waits on this so the old capturer-pool and lease teardown finish before
    /// the successor starts.
    pub media_exited: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Bumped as each media thread is started; less [`Self::media_exited`], the threads still
    /// alive, which the flags cannot say once `end_session` has lowered them.
    pub media_started: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Client IDR / reference-frame invalidation request. Video thread forces a keyframe and clears it.
    pub force_idr: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Client 0x0301 lost-frame range. Video thread drains it into `Encoder::invalidate_ref_frames`,
    /// falling back to a full IDR when the encoder cannot invalidate. `None` = nothing pending.
    pub rfi_range: std::sync::Arc<std::sync::Mutex<Option<(i64, i64)>>>,
    /// Cumulative `0x0201` loss-stats from GameStream's control stream. The video thread's
    /// 1 Hz step reads window deltas.
    pub loss_stats: std::sync::Arc<crate::gamestream::GsLossStats>,
    /// This session's input tallies, bumped by GameStream's control stream and read by the
    /// summary. One session at a time on this plane, so the video thread clears them at
    /// stream start — without that, `session.ended` would report zeros nobody counted.
    pub counters: Arc<crate::session_status::SessionCounters>,
    /// Persistent audio capturer. Reused when channel count matches (drained so no stale
    /// audio is sent); dropped and reopened when a session negotiates a different count.
    pub audio_cap: std::sync::Arc<std::sync::Mutex<Option<Box<dyn crate::audio::AudioCapturer>>>>,
    /// Shared streaming-stats recorder. The same `Arc` is handed to mgmt, GameStream, and
    /// native loops so one capture spans whichever path is streaming.
    pub stats: Arc<crate::stats_recorder::StatsRecorder>,
    /// Per-client access grants, keyed by certificate fingerprint hex. Same registry as the
    /// native trust store. Set once by [`serve`]; if unset, every paired peer is ungoverned.
    pub access: std::sync::OnceLock<Arc<crate::native_pairing::NativePairing>>,
    /// The people on this box. Set once by [`serve`] with the owner ensured.
    pub profiles: std::sync::OnceLock<Arc<crate::profiles::Profiles>>,
    /// The native port clients dial, for the profile list's `seat.port`. Set once by [`serve`].
    pub native_port: std::sync::OnceLock<u16>,
    #[cfg(feature = "gamestream")]
    pub gs: crate::gamestream::GsState,
}

impl AppState {
    /// Fresh host state. The paired allow-list is loaded from disk; `stats` is the shared
    /// recorder handed to mgmt and the streaming loops.
    pub fn new(
        host: Host,
        stats: Arc<crate::stats_recorder::StatsRecorder>,
        #[cfg(feature = "gamestream")] gs: crate::gamestream::GsState,
    ) -> AppState {
        AppState {
            host,
            paired: std::sync::Mutex::new(crate::gamestream::load_paired()),
            launch: std::sync::Mutex::new(None),
            streaming: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            quit: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            audio_streaming: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            force_idr: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            rfi_range: std::sync::Arc::new(std::sync::Mutex::new(None)),
            loss_stats: std::sync::Arc::new(crate::gamestream::GsLossStats::default()),
            counters: Arc::new(crate::session_status::SessionCounters::default()),
            media_exited: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            media_started: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            audio_cap: std::sync::Arc::new(std::sync::Mutex::new(None)),
            stats,
            access: std::sync::OnceLock::new(),
            profiles: std::sync::OnceLock::new(),
            native_port: std::sync::OnceLock::new(),
            #[cfg(feature = "gamestream")]
            gs,
        }
    }

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
        self.gs
            .stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
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
    // exists, the console starts and serves the SAN-less RSA cert.
    let native_ident = crate::identity::load_or_adopt(&np).context("native host identity")?;
    #[cfg(feature = "gamestream")]
    let gs = crate::gamestream::GsState::new(
        crate::gamestream::cert::ServerIdentity::load_or_create().context("host certificate")?,
    );
    let state = Arc::new(AppState::new(
        host,
        stats.clone(),
        #[cfg(feature = "gamestream")]
        gs,
    ));
    // Hand GameStream the grants registry so nvhttp launch and ENet resolve a Moonlight
    // fingerprint against the same mask the native plane enforces.
    let _ = state.access.set(np.clone());
    let profiles = match pf_paths::seat::trust_dir() {
        // A seat host reads the box's profiles; the box creates the owner and migrates.
        Some(dir) => {
            let seat = pf_paths::seat::seat_id().map_err(anyhow::Error::msg)?;
            crate::profiles::Profiles::load_box(dir.join("profiles.json"), seat)
        }
        None => {
            let profiles = crate::profiles::Profiles::load_with(None, None);
            if let Err(e) = profiles.ensure_owner(&state.host.hostname) {
                tracing::warn!(error = %format!("{e:#}"), "owner profile not created");
            }
            profiles
        }
    };
    #[cfg(target_os = "linux")]
    {
        // The door keeps no seat homes of its own: they are the owner's host's, and move there.
        if pf_paths::seat::trust_dir().is_none() && !pf_paths::seat::is_door() {
            let paired: Vec<(String, String)> = np
                .list()
                .into_iter()
                .map(|c| (c.name, c.fingerprint))
                .collect();
            profiles.migrate_device_seats(&pf_paths::seats_dir(), &paired);
        }
    }
    // A seat host is pairing-required whatever its flags say.
    let native = crate::native::NativeServe {
        require_pairing: native.require_pairing || pf_paths::seat::is_seat_host(),
        ..native
    };
    let profiles = Arc::new(profiles);
    let _ = state.profiles.set(profiles.clone());
    let _ = state.native_port.set(native.port);
    tracing::info!(
        hostname = %state.host.hostname,
        uniqueid = %state.host.uniqueid,
        ip = %state.host.local_ip(),
        native_port = native.port,
        require_pairing = native.require_pairing,
        gamestream,
        door = pf_paths::seat::is_door(),
        "punktfunk host"
    );
    crate::net_health::log_addresses();
    crate::net_health::spawn_route_watch();
    // The door keeps the seats of recent players up and lets idle ones go, as the Windows service
    // does for its own.
    #[cfg(target_os = "linux")]
    if pf_paths::seat::is_door() {
        crate::seats::lifecycle::run_door();
    }
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
        // A Linux seat answers "Return to Gaming Mode" itself, so the box's login is never asked.
        #[cfg(target_os = "linux")]
        crate::seats::session_switch::spawn();
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
                let _advert = crate::gamestream::start(&state, native.mdns)?;
                tracing::info!(
                    port = native.port,
                    "unified host: GameStream/Moonlight compat + native punktfunk/1 (QUIC)"
                );
                tokio::try_join!(
                    crate::gamestream::nvhttp::run(state.clone()),
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
                        profiles.clone(),
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
                    profiles.clone(),
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

/// Load the persisted host uniqueid, or mint 16 random bytes as hex and store it.
fn load_or_create_uniqueid() -> Result<String> {
    let path = pf_paths::config_dir().join("uniqueid");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let t = s.trim();
        if !t.is_empty() {
            return Ok(t.to_string());
        }
    }
    let id = hex::encode(rand::random::<[u8; 16]>());
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
            http_port: crate::gamestream::HTTP_PORT,
            https_port: crate::gamestream::HTTPS_PORT,
            os_chain: "linux".into(),
            os_name: "Linux".into(),
        };
        let stats = crate::stats_recorder::StatsRecorder::new(crate::test_support::scratch());
        AppState::new(
            host,
            stats,
            #[cfg(feature = "gamestream")]
            crate::gamestream::GsState::new(
                crate::gamestream::cert::ServerIdentity::ephemeral().expect("ephemeral identity"),
            ),
        )
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
        *state.launch.lock().unwrap() = Some(crate::gamestream::LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 0,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            host_audio: false,
            peer_ip: None,
            owner_fp: None,
        });
        #[cfg(feature = "gamestream")]
        {
            *state.gs.stream.lock().unwrap() = Some(crate::gamestream::stream::StreamConfig {
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
        assert!(state.gs.stream.lock().unwrap().is_none());

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
