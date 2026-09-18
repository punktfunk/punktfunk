//! Native `punktfunk/1` host: QUIC control plane plus the core data plane over UDP.
//!
//! Welcome negotiates GF(2¹⁶) Leopard FEC and AES-GCM. Hello names a display mode;
//! the host opens a virtual output at that size/refresh (same backends as GameStream).
//! Input arrives as QUIC datagrams into the session injector. Video AUs carry
//! wall-clock `pts_ns`. Concurrent sessions share host-lifetime audio/input/mic;
//! isolated gamescope spawns do not. Data plane is native threads, not async.
//! A session also carries desktop Opus (`AUDIO_MAGIC`) and gamepads (`RUMBLE_MAGIC`).
//!
//! Serves `~/.config/punktfunk/cert.pem` (shared with GameStream pairing) and logs
//! the SHA-256 fingerprint clients pin. `punktfunk-probe --connect host:9777` is
//! the counterpart. Evidence: `design/` and the tests below.

use anyhow::{anyhow, Context, Result};
// The wire budget, adaptive FEC and their band: one arithmetic, shared with
// the client's controller and the link simulator.
use punktfunk_core::abr::budget::{
    budget_kbps_for_encoder, encoder_kbps_for_budget, fec_target, FEC_ADAPTIVE_START,
    MIN_BITRATE_KBPS,
};
use punktfunk_core::config::{CompositorPref, FecConfig, FecScheme, GamepadPref, Role};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::packet::{FLAG_PIC, FLAG_PROBE, FLAG_SOF};
use punktfunk_core::quic::{
    classify, endpoint, io, AccessUpdate, AckReason, BitrateChanged, ClockEcho, ClockProbe,
    ColorInfo, GrantClass, Hello, LossReport, PairRequest, PipelineGap, ProbeRequest, ProbeResult,
    Reconfigure, Reconfigured, RequestKeyframe, RfiRequest, SetBitrate, Start, Welcome, GRANT_ALL,
    GRANT_CLIPBOARD, GRANT_GAMEPAD, GRANT_LAUNCH, GRANT_MIC, GRANT_POINTER,
};
use punktfunk_core::transport::UdpTransport;
use punktfunk_core::Session;
use rand::RngCore;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

/// Shared with GameStream.
pub(crate) use pf_frame::thread_qos::boost_thread_priority;

mod compositor;
// The session's control connection, whichever transport carries it (quinn or WebTransport).
pub(crate) mod link;
use compositor::resolve_compositor;

/// GameStream presents the same virtual pad and must pick `windows_xbox_hid` from this definition.
pub(crate) mod gamepad;
use gamepad::{resolve_gamepad, resolve_pad_kind, route_decision};

mod pairing;
pub(crate) use pairing::pair_ceremony;

mod audio;
use audio::audio_thread;

/// Per-pad DualSense audio (0xD1 → `PAD_AUDIO_MAGIC`). The input thread spawns one
/// streamer per pad; Welcome advertises the cap via `pad_audio::host_cap`.
mod pad_audio;

mod input;
/// Per-pad motion inter-arrival ([`motion_cadence::MotionCadence`]), logged at session end.
mod motion_cadence;
/// Controller updates reaching the host ([`pad_uplink::PadUplink`]): the client → host link.
mod pad_uplink;
use input::{input_thread, ClientInput};

mod handshake;
/// `PUNKTFUNK_WIRE_MTU`, the control-connection path-MTU watch, and the per-peer shard clamp.
mod wire_mtu;

mod control;
mod cursor_fwd;

mod stream;
use stream::{reconfig_allowed, software_stream, synthetic_stream, virtual_stream, SessionContext};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Punktfunk1Source {
    /// Protocol-test frames; the client byte-checks the payload.
    Synthetic,
    /// Virtual display at the requested mode → NVENC.
    Virtual,
    /// A moving test picture through the software H.264 encoder, unbounded. No display and no
    /// GPU: what a headless host serves so a real client can be proved against it.
    Software,
}

pub struct Punktfunk1Options {
    pub port: u16,
    pub source: Punktfunk1Source,
    pub seconds: u32,
    pub frames: u32,
    /// `0` = serve forever.
    pub max_sessions: u32,
    /// Simultaneous streams (NVENC/GPU bound). Shared-desktop backends share host-lifetime
    /// input/audio/mic; isolated gamescope spawns do not (`design/gamescope-multiuser.md`).
    /// `0` = unlimited. Overflow waits in the accept queue.
    pub max_concurrent: usize,
    /// Paired-fingerprint gate. Implies `allow_pairing` — a host that requires pairing
    /// must still accept ceremonies.
    pub require_pairing: bool,
    /// Accept PairRequests. Default off; `require_pairing` forces this on.
    pub allow_pairing: bool,
    /// Tests: fixed PIN. `None` = a fresh random 4-digit PIN per ceremony.
    pub pairing_pin: Option<String>,
    /// Tests: store path. `None` = the default config path.
    pub paired_store: Option<std::path::PathBuf>,
    /// Fixed data-plane UDP port. `None`/`Some(0)`: an ephemeral port per session.
    /// `Some(p)`: bind `p` (busy → ephemeral, [`bind_data_socket`]). Either way the
    /// client's hole-punch picks the return path; a fixed port only gives a firewall or
    /// a port proxy one number to open.
    pub data_port: Option<u16>,
    /// Disconnect-detection latency. `None` = core default (8 s). From
    /// `PUNKTFUNK_IDLE_TIMEOUT_MS`; ≥1 s floor, keep-alive scales so a live session
    /// never false-closes.
    pub idle_timeout: Option<std::time::Duration>,
    /// `_punktfunk._udp` advert. `--no-mdns` / `PUNKTFUNK_MDNS=0` skips it.
    pub mdns: bool,
}

/// Bind the per-session data-plane UDP socket ([`Punktfunk1Options::data_port`]): the
/// fixed port when set and free, else an ephemeral one. Held from handshake through
/// streaming — no drop-then-rebind window that could steal a fixed port. It never decides
/// how video is addressed: every session waits for the client's hole-punch on whatever
/// port this bound, because a fixed port fixes only the host's side of the path — the
/// client's NAT, or a port proxy such as Porthole, still remaps the source to answer.
///
/// `local_ip` is the address the QUIC connection was received on. Bind it: the client's
/// data socket is `connect`ed to the host IP it dialed, and its kernel drops any other
/// source. A wildcard bind lets the routing table pick a different egress on a
/// multi-homed host. `None` or a bind failure falls back to the wildcard.
fn bind_data_socket(
    data_port: Option<u16>,
    local_ip: Option<std::net::IpAddr>,
) -> std::io::Result<std::net::UdpSocket> {
    // Dual-stack endpoints report IPv4-mapped v6; unmap so a v4 `connect` can bind.
    let local_ip = local_ip.map(|ip| match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, std::net::IpAddr::V4),
        v4 => v4,
    });
    let wildcard = |ip: Option<std::net::IpAddr>| {
        ip.unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
    };
    if let Some(p) = data_port.filter(|p| *p != 0) {
        match std::net::UdpSocket::bind((wildcard(local_ip), p)) {
            Ok(sock) => return Ok(sock),
            Err(e) => tracing::warn!(
                data_port = p,
                error = %e,
                "fixed --data-port is busy (a concurrent session already holds it?) — \
                 falling back to a random port + hole-punch for this session"
            ),
        }
    }
    match std::net::UdpSocket::bind((wildcard(local_ip), 0)) {
        Ok(sock) => Ok(sock),
        // The control connection arrived here; a bind failure means the adapter dropped.
        // Wildcard still reaches a routable client.
        Err(e) if local_ip.is_some() => {
            tracing::warn!(
                local_ip = ?local_ip,
                error = %e,
                "data plane did not bind to the control-connection address — falling back to the \
                 wildcard; on a multi-homed host video may egress from a different interface than \
                 the client dialed, which it silently drops"
            );
            Ok(std::net::UdpSocket::bind("0.0.0.0:0")?)
        }
        Err(e) => Err(e),
    }
}

use crate::native_pairing::{NativePairing, PairingDecision};
use crate::send_pacing::{percentile, PaceStat};
use crate::stats_recorder::StatsRecorder;

/// Bounds online PIN guessing: SPAKE2 already gives one guess per ceremony; this caps the rate.
pub(crate) const PAIRING_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(2);

/// `u32 LE index` then `data[i] = idx + i` (wrapping) — the client byte-checks this.
pub fn test_frame(idx: u32, len: usize) -> Vec<u8> {
    let mut d = vec![0u8; len];
    d[0..4].copy_from_slice(&idx.to_le_bytes());
    for (i, b) in d.iter_mut().enumerate().skip(4) {
        *b = (idx as u8).wrapping_add(i as u8);
    }
    d
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Unix seconds. Access deadlines are stored and checked in wall time, not a cached
/// monotonic offset, so an NTP step moves a deadline with the clock.
fn wall_unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Remaining lifetime on the wire: saturating whole seconds, floor 1. `0` means *permanent*,
/// so a deadline due this second still advertises as expiring.
fn remaining_secs_wire(deadline: Option<i64>, now: i64) -> u32 {
    deadline
        .map(|d| u32::try_from((d - now).max(1)).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

pub fn run(opts: Punktfunk1Options) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("tokio runtime")?;
    // Standalone CLI arms at startup from the flags (PIN is logged). `serve --native` arms on demand.
    let np = Arc::new(NativePairing::load_with(
        opts.paired_store.clone(),
        opts.pairing_pin.clone(),
        opts.allow_pairing || opts.require_pairing,
    )?);
    // No mgmt API here, so the recorder stays disarmed (`is_armed()` is always false).
    let stats = StatsRecorder::new(crate::stats_recorder::default_dir());
    // Standalone resolves identity itself; unified `serve` does it once for both planes.
    let ident = crate::identity::load_or_adopt(&np).context("native host identity")?;
    // No management API → advertise no `mgmt` port (0).
    rt.block_on(serve(opts, 0, np, stats, ident, None))
}

/// [`run`] with an in-memory identity. Tests must not mint `native-cert.pem` in the real
/// config dir: a live host on the same box would adopt it and strand every pinned client.
#[cfg(test)]
fn run_ephemeral(opts: Punktfunk1Options) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("tokio runtime")?;
    let np = Arc::new(NativePairing::load_with(
        opts.paired_store.clone(),
        opts.pairing_pin.clone(),
        opts.allow_pairing || opts.require_pairing,
    )?);
    let stats = StatsRecorder::new(crate::stats_recorder::default_dir());
    let ident = crate::identity::ephemeral()?;
    rt.block_on(serve(opts, 0, np, stats, ident, None))
}

fn fingerprint_hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// Native host config when unified `serve` runs it in-process.
pub(crate) struct NativeServe {
    pub port: u16,
    /// Default on. `serve --open` turns it off. Pairing is armed on demand from the console.
    pub require_pairing: bool,
    /// Management API TCP port, advertised over mDNS so a client browses the library on this IP.
    pub mgmt_port: u16,
    /// [`Punktfunk1Options::data_port`]. `None` = ephemeral + hole-punch.
    pub data_port: Option<u16>,
    /// Gates `_punktfunk._udp` and GameStream `_nvstream` together. See [`Punktfunk1Options::mdns`].
    pub mdns: bool,
    /// Where the browser plane listens, or `None` when it is off — which is the default
    /// (`--webtransport` / `PUNKTFUNK_WEBTRANSPORT`). See `crate::webtransport`.
    pub webtransport_bind: Option<std::net::SocketAddr>,
}

/// NVENC session cap (high-res split-encode holds two). Overflow waits in the accept queue.
pub(crate) const DEFAULT_MAX_CONCURRENT: usize = 4;

/// `PUNKTFUNK_IDLE_TIMEOUT_MS`; `None` (unset/invalid/zero) = core default (8 s). Clamped
/// downstream to ≥1 s with a keep-alive that scales, so a live session never false-closes.
pub(crate) fn idle_timeout_from_env() -> Option<std::time::Duration> {
    pf_host_config::knob("PUNKTFUNK_IDLE_TIMEOUT_MS")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(std::time::Duration::from_millis)
}

/// `PUNKTFUNK_SOURCE=software`: a software-encoded test picture instead of a display, so a
/// headless box with the management API can prove a client end to end. Anything else is the
/// display.
fn source_from_env() -> Punktfunk1Source {
    match std::env::var("PUNKTFUNK_SOURCE").as_deref() {
        Ok("software") => Punktfunk1Source::Software,
        _ => Punktfunk1Source::Virtual,
    }
}

pub(crate) fn native_serve_opts(cfg: &NativeServe) -> Punktfunk1Options {
    Punktfunk1Options {
        port: cfg.port,
        source: source_from_env(),
        seconds: 7 * 24 * 3600, // 7 days: a cap, not a cut of a live stream
        frames: 0,
        max_sessions: 0,
        max_concurrent: DEFAULT_MAX_CONCURRENT,
        require_pairing: cfg.require_pairing,
        allow_pairing: false,
        pairing_pin: None,
        paired_store: None,
        data_port: cfg.data_port,
        idle_timeout: idle_timeout_from_env(),
        mdns: cfg.mdns,
    }
}

pub(crate) async fn serve(
    opts: Punktfunk1Options,
    mgmt_port: u16,
    np: Arc<NativePairing>,
    stats: Arc<StatsRecorder>,
    // Caller-resolved so the planes cannot race adoption (P-256 vs legacy RSA).
    identity: crate::identity::NativeIdentity,
    // The browser plane, when the operator asked for it. Spawned from here, not joined: a browser
    // runs this plane's session on this plane's capturer, injector and session pool.
    web: Option<crate::webtransport::Plane>,
) -> Result<()> {
    let fingerprint = endpoint::fingerprint_of_pem(&identity.cert_pem)
        .map_err(|e| anyhow!("cert fingerprint: {e}"))?;
    let ep = endpoint::server_with_identity_idle(
        ([0, 0, 0, 0], opts.port).into(),
        &identity.cert_pem,
        &identity.key_pem,
        opts.idle_timeout.unwrap_or(endpoint::DEFAULT_IDLE_TIMEOUT),
    )
    .map_err(|e| anyhow!("QUIC server endpoint: {e}"))?;
    tracing::info!(
        port = opts.port,
        source = ?opts.source,
        fingerprint = %fingerprint_hex(&fingerprint),
        "punktfunk/1 host listening (QUIC) — clients pin this fingerprint"
    );

    // Held for the host lifetime — dropping `_advert` unregisters. Best-effort: a
    // discovery failure must not stop streaming (`--connect HOST:PORT` still works).
    let _advert = if !opts.mdns {
        tracing::info!(
            "mDNS advertisement disabled (--no-mdns / PUNKTFUNK_MDNS) — clients connect by address"
        );
        None
    } else {
        match crate::gamestream::Host::detect() {
        Ok(h) => crate::discovery::advertise_native(
            &h.hostname,
            opts.port,
            &fingerprint_hex(&fingerprint),
            opts.require_pairing,
            &h.uniqueid,
            // 0 = standalone (no mgmt API) → do not advertise an `mgmt` port.
            (mgmt_port != 0).then_some(mgmt_port),
            &h.os_chain,
        )
        .map_err(|e| tracing::warn!(error = %format!("{e:#}"), "native mDNS advertise failed (continuing)"))
        .ok(),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "host detect for mDNS failed (continuing)");
            None
        }
        }
    };

    // Host-lifetime capturer: one PipeWire stream, handed session to session (`AudioCapSlot`).
    let audio_cap: AudioCapSlot = Arc::new(std::sync::Mutex::new(None));
    // Host-lifetime injector: one RemoteDesktop-portal grant. A CreateSession per session
    // races portal teardown on reconnect and wedges KWin EIS. Gamepads stay per-session.
    let injector = crate::inject::InjectorService::start();
    // Host-lifetime virtual mic ([`crate::audio::MicPump`]): 0xCB Opus → a persistent source
    // games can bind before they launch. Opens eagerly; self-heals if the backend dies.
    let mic_service = crate::audio::MicPump::start();
    // Windows (`PUNKTFUNK_PAD_AUDIO` / `_SLOTS`): pre-provision DualSense speaker endpoints
    // once. A stored-but-not-served stamp triggers one Audiosrv restart before any session.
    // Failure logs once and leaves pads working without pad audio.
    #[cfg(target_os = "windows")]
    crate::audio::pad_endpoint::provision_at_startup(true);
    // Windows: mint "Punktfunk Speakers/Microphone" (Valve streaming drivers). Best-effort;
    // without Steam's drivers the wiring plan keeps its name-based ladder.
    #[cfg(target_os = "windows")]
    crate::audio::minted::provision_at_startup();
    // Debounced TV-session restore on idle, not per-disconnect. Dropping this stops it.
    let _restore_worker = crate::vdisplay::start_restore_worker();
    // Recover a takeover stranded by a crashed previous instance (`$XDG_RUNTIME_DIR`).
    crate::vdisplay::restore_takeover_on_startup();
    // Takeover needs the host user in `punktfunk`. Missing membership degrades to mirroring.
    // No-op off Linux.
    crate::vdisplay::preflight_takeover_privilege();
    // Console registry after the probed subsystems are up, so a probe never names a node
    // that was about to appear.
    crate::diagnostics::preflight();
    install_shutdown_restore();
    // Headless CLI: surface the PIN if armed at startup. The console arms on demand.
    let st = np.status();
    if let Some(pin) = &st.pin {
        tracing::info!(
            paired = st.paired_clients,
            require = opts.require_pairing,
            "pairing armed — enter the PIN shown on the console to pair a client"
        );
        // Shared secret: print to the operator's terminal, not tracing — GET /api/v1/logs
        // ships the DEBUG ring.
        eprintln!("[punktfunk] pairing PIN: {pin}  (enter this on the client to pair)");
    }
    let last_pairing = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
    let opts = Arc::new(opts);

    // Permit taken before accept: overflow waits in QUIC's backlog. `0` = unlimited.
    // Handshake + pipeline run in the spawned task so a slow client never blocks accept.
    let permits = match opts.max_concurrent {
        0 => tokio::sync::Semaphore::MAX_PERMITS,
        n => n,
    };
    let sem = Arc::new(tokio::sync::Semaphore::new(permits));
    // Secondary tier: a port it cannot bind must not take the streaming host down. Loud, though.
    if let Some(plane) = web {
        let host = SessionHost {
            opts: Arc::clone(&opts),
            audio_cap: audio_cap.clone(),
            inj_tx: injector.sender(),
            mic_tx: mic_service.sender(),
            np: np.clone(),
            stats: stats.clone(),
        };
        let (bind, sem) = (plane.bind, sem.clone());
        tokio::spawn(async move {
            if let Err(e) = crate::webtransport::serve(plane, host, sem).await {
                tracing::error!(%bind, error = %e, "WebTransport plane stopped");
            }
        });
    }
    let mut sessions = tokio::task::JoinSet::new();
    let max_sessions = opts.max_sessions;
    // Handshakes that completed. `--max-sessions` counts those, not attempts, so the task that
    // finishes the N-th one wakes the accept loop through `done`.
    let accepted = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let done = Arc::new(tokio::sync::Notify::new());
    tracing::info!(
        max_concurrent = opts.max_concurrent,
        "accepting sessions (concurrent)"
    );

    loop {
        let incoming = tokio::select! {
            i = ep.accept() => match i {
                Some(i) => i,
                None => break,
            },
            () = done.notified(), if max_sessions != 0 => break,
        };
        // A source that has not proved it can receive at the address it claims gets a Retry
        // rather than a task and a TLS handshake, so a spoofed-source flood cannot make this
        // host amplify. Costs the real client one RTT on first contact. A failed retry drops
        // `Incoming`, which refuses.
        if !incoming.remote_address_validated() {
            let _ = incoming.retry();
            continue;
        }
        let opts = opts.clone();
        let audio_cap = audio_cap.clone();
        let np = np.clone();
        let last_pairing = last_pairing.clone();
        let stats = stats.clone();
        let inj_tx = injector.sender();
        let mic_tx = mic_service.sender();
        let sem = sem.clone();
        let accepted = accepted.clone();
        let done = done.clone();
        sessions.spawn(async move {
            // Handshake off the accept loop: a peer that stalls it holds only its own task, not
            // every other client, up to the idle timeout. A pin mismatch still ends here, before
            // a session slot is taken.
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "QUIC accept failed");
                    return;
                }
            };
            let n = accepted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if max_sessions != 0 && n >= max_sessions {
                done.notify_one();
            }
            // Slot after handshake: a full host still accepts, so the waiter sees a live path
            // (keep-alive) instead of a silent dial timeout.
            let permit = sem
                .clone()
                .acquire_owned()
                .await
                .expect("session semaphore is never closed");
            let peer = conn.remote_address();
            tracing::info!(%peer, "punktfunk/1 client connected");
            // `serve_session` owns the permit: released while a knock is parked, re-acquired on
            // approval. A setup failure still needs a typed close (cheap clone).
            let sem_session = sem;
            let conn_err = conn.clone();
            match serve_session(
                conn.into(),
                &opts,
                &audio_cap,
                inj_tx,
                mic_tx,
                &fingerprint,
                &np,
                &last_pairing,
                stats,
                permit,
                sem_session,
            )
            .await
            {
                Ok(Served::Session) => tracing::info!(%peer, "session complete"),
                Ok(Served::ProbeClose) => tracing::debug!(
                    %peer,
                    "closed before the control handshake (reachability probe)"
                ),
                Err(e) => {
                    // Typed setup-failed close so the client does not see a bare mid-frame drop.
                    // First-wins: a gate that already closed, or a peer close, makes this a no-op.
                    // The reason bytes are read by a person, so they carry the user sentence and
                    // the operator chain stays in the log.
                    let detail = format!("{e:#}");
                    conn_err.close(
                        punktfunk_core::reject::SETUP_FAILED_CLOSE_CODE.into(),
                        setup_failed_sentence(&e).unwrap_or_default().as_bytes(),
                    );
                    tracing::warn!(%peer, error = %detail, "session ended with error")
                }
            }
        });
    }
    // Drain in-flight sessions (max_sessions reached or endpoint closed).
    while sessions.join_next().await.is_some() {}
    ep.wait_idle().await;
    Ok(())
}

/// Shutdown wait for the box's session to come back. Bounds a wedge; well inside systemd's
/// 90 s `TimeoutStopSec`.
const SHUTDOWN_RESTORE_GRACE: std::time::Duration = std::time::Duration::from_secs(20);

/// Catch `SIGTERM`/`SIGINT`, restore the box's session, then exit. A takeover that stopped
/// the display manager leaves no graphical session if killed; crash-restore lives in
/// `$XDG_RUNTIME_DIR`, which logind removes with the user manager. Blocking restore under
/// [`SHUTDOWN_RESTORE_GRACE`]; a host that took nothing over exits immediately.
fn install_shutdown_restore() {
    #[cfg(unix)]
    tokio::spawn(async {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            tracing::warn!(
                "shutdown signal handlers did not install — a host stopped mid-takeover leaves \
                 the box's own session down until it is restarted"
            );
            return;
        };
        let sig = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = int.recv() => "SIGINT",
        };
        tracing::info!(
            signal = sig,
            "host stopping — handing the box's session back"
        );
        let restore = tokio::task::spawn_blocking(crate::vdisplay::restore_takeover_now);
        if tokio::time::timeout(SHUTDOWN_RESTORE_GRACE, restore)
            .await
            .is_err()
        {
            tracing::warn!(
                secs = SHUTDOWN_RESTORE_GRACE.as_secs(),
                "the session restore did not finish in time — exiting anyway"
            );
        }
        std::process::exit(0);
    });
}

/// Bound the control phase; an unfinished handshake would otherwise wedge the host.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Wait after `stop` before [`serve_session`] abandons the stream thread.
/// Capture-loss rebuild is 40 s; a cold pipeline-build can take ~10 s. 90 s leaves headroom.
const STREAM_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(90);

/// Audio/input join after close. Both poll `stop`: audio every ≤5 s, input every ≤4 ms.
/// This only catches a wedge.
const SIDE_THREAD_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Resolves once `stop` has been set for [`STREAM_STOP_GRACE`].
/// Polled: `stop` is a plain flag shared with blocking threads (500 ms, one relaxed load).
async fn stop_overdue(stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    tokio::time::sleep(STREAM_STOP_GRACE).await;
}

/// `mode_conflict = reject` close code. Distinct from transport failure (`RejectReason::Busy`).
const REJECT_BUSY_CODE: u32 = punktfunk_core::reject::REJECT_BUSY_CLOSE_CODE;

/// Close with the typed reject code before the session task returns `Err`. A bare drop
/// closes with code 0, which the client cannot tell from transport trouble.
fn close_rejected(conn: &link::SessionLink, reason: punktfunk_core::reject::RejectReason) {
    conn.close(reason.close_code(), reason.to_string().as_bytes());
}

/// One counter and one `warn!` per grant class per session. Totals at end-of-stream;
/// per-event logging would be a log DoS.
struct GrantDrops {
    // One slot per grant bit (7 with `Power`). Power never drops input; `idx` must still
    // stay in bounds for every `GrantClass`.
    counts: [AtomicU64; 7],
    warned: [AtomicBool; 7],
}

impl GrantDrops {
    fn new() -> GrantDrops {
        GrantDrops {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            warned: std::array::from_fn(|_| AtomicBool::new(false)),
        }
    }

    /// Bit position of the grant, so the table cannot drift from the wire vocabulary.
    fn idx(class: GrantClass) -> usize {
        class.bit().trailing_zeros() as usize
    }

    /// Count one drop; log only the first of each class.
    fn note(&self, class: GrantClass) {
        let i = Self::idx(class);
        self.counts[i].fetch_add(1, Ordering::Relaxed);
        if !self.warned[i].swap(true, Ordering::Relaxed) {
            tracing::warn!(
                class = ?class,
                "dropping client input this session's access grants don't cover — counted; \
                 further drops of this class are silent until the session-end totals"
            );
        }
    }

    /// `Class=count` pairs; `"none"` when nothing was dropped.
    fn summary(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for class in [
            GrantClass::Gamepad,
            GrantClass::Pointer,
            GrantClass::Keyboard,
            GrantClass::Clipboard,
            GrantClass::Mic,
            GrantClass::Launch,
        ] {
            let n = self.counts[Self::idx(class)].load(Ordering::Relaxed);
            if n != 0 {
                if !out.is_empty() {
                    out.push(' ');
                }
                let _ = write!(out, "{class:?}={n}");
            }
        }
        if out.is_empty() {
            out.push_str("none");
        }
        out
    }
}

/// Seconds before the deadline for best-effort toasts (T−5 m, T−1 m). Older clients miss them.
const ACCESS_WARN_SECS: [i64; 2] = [300, 60];

/// Thresholds already behind the deadline at `now` are spent, not fired — at admission
/// (Welcome just advertised remaining) and after an edit (`AccessUpdate` just did). A
/// threshold only fires by being crossed live.
fn spent_warnings(deadline: Option<i64>, now: i64) -> [bool; 2] {
    match deadline {
        None => [true, true],
        Some(d) => [
            d - now <= ACCESS_WARN_SECS[0],
            d - now <= ACCESS_WARN_SECS[1],
        ],
    }
}

/// Sleep until the next unfired boundary, re-derived from `deadline − now` each lap and
/// capped at 30 s so an NTP step moves the deadline within one cap interval.
fn access_sleep(deadline: Option<i64>, warned: &[bool; 2], now: i64) -> std::time::Duration {
    let Some(d) = deadline else {
        // Permanent: park; the watch/close arms wake.
        return std::time::Duration::from_secs(3600);
    };
    let mut next = d;
    for (i, w) in ACCESS_WARN_SECS.iter().enumerate() {
        if !warned[i] {
            next = next.min(d - w);
        }
    }
    std::time::Duration::from_secs((next - now).clamp(1, 30) as u64)
}

/// Per-session access: expiry deadline + watch. Best-effort `AccessUpdate` at T−5 m / T−1 m
/// and on every grant edit; folds the live mask within one event; typed-close at deadline,
/// "expire now", or unpair. Closes only this connection — the owner's stream is untouched.
///
/// A pairing edit wins over a console per-session re-point: this task rewrites both the live
/// mask and the ceiling the route clamps to.
async fn access_lifecycle(
    conn: link::SessionLink,
    mut watch_rx: tokio::sync::watch::Receiver<crate::native_pairing::AccessState>,
    controls: crate::session_status::SessionControls,
    clip_enabled: Arc<AtomicBool>,
    access_tx: tokio::sync::mpsc::UnboundedSender<AccessUpdate>,
    mut deadline: Option<i64>,
    device: crate::events::DeviceRef,
) {
    let mut warned = spent_warnings(deadline, wall_unix_now());
    // `power.*` ending every session: typed close so the client does not see a transport error.
    let mut power_rx = crate::power::closing_rx();
    loop {
        let now = wall_unix_now();
        if let Some(d) = deadline {
            if now >= d {
                // Wall clock at fire: `d − now` is recomputed each lap, so an NTP step moves it.
                tracing::info!(
                    device = %device.name,
                    fingerprint = %device.fingerprint,
                    "temporary access expired — closing this device's session"
                );
                crate::events::emit(crate::events::EventKind::AccessExpired { device });
                close_rejected(&conn, punktfunk_core::reject::RejectReason::AccessExpired);
                return;
            }
            let remaining = d - now;
            for (i, w) in ACCESS_WARN_SECS.iter().enumerate() {
                if !warned[i] && remaining <= *w {
                    warned[i] = true;
                    let _ = access_tx.send(AccessUpdate {
                        grants: controls.grants.load(Ordering::Relaxed),
                        remaining_secs: u32::try_from(remaining).unwrap_or(u32::MAX),
                    });
                }
            }
        }
        tokio::select! {
            () = tokio::time::sleep(access_sleep(deadline, &warned, wall_unix_now())) => {}
            changed = watch_rx.changed() => {
                if changed.is_err() {
                    return; // registry gone — host shutting down
                }
                let st = *watch_rx.borrow_and_update();
                if st.revoked {
                    // Unpair is terminal: end the session, do not merely mute it.
                    tracing::info!(
                        device = %device.name,
                        fingerprint = %device.fingerprint,
                        "device unpaired — closing its live session"
                    );
                    close_rejected(&conn, punktfunk_core::reject::RejectReason::AccessExpired);
                    return;
                }
                // Live mask updates now; the datagram filter reads it on the next event.
                // Wider-mask resources stay up and starve (tearing a live uinput pad is churn).
                // Clipboard is the cheap exception: clear the flag, stop forwarding copies.
                controls.grants.store(st.grants, Ordering::Relaxed);
                controls.ceiling.store(st.grants, Ordering::Relaxed);
                if st.grants & GRANT_CLIPBOARD == 0 {
                    clip_enabled.store(false, Ordering::SeqCst);
                }
                deadline = st.deadline_unix;
                controls
                    .deadline_unix
                    .store(deadline.unwrap_or(0), Ordering::Relaxed);
                let now = wall_unix_now();
                warned = spent_warnings(deadline, now);
                // Skip an "expire now" (deadline already past) so we do not advertise a phantom second.
                if deadline.is_none_or(|d| d > now) {
                    let _ = access_tx.send(AccessUpdate {
                        grants: st.grants,
                        remaining_secs: remaining_secs_wire(deadline, now),
                    });
                }
            }
            changed = power_rx.changed() => {
                if changed.is_ok() && *power_rx.borrow_and_update() {
                    close_rejected(&conn, punktfunk_core::reject::RejectReason::HostPower);
                    return;
                }
            }
            _ = conn.closed() => return,
        }
    }
}

/// Client close code for a deliberate quit (user "stop"). Tears the virtual display down
/// immediately, skipping the keep-alive linger. Any other close still lingers for reconnect.
const QUIT_CODE: u32 = punktfunk_core::quic::QUIT_CLOSE_CODE;

/// Fallback when `Hello::bitrate_kbps == 0` (20 Mbps). A client that knows its link asks.
const DEFAULT_BITRATE_KBPS: u32 = 20_000;
/// Ceiling on a resolved rate: headroom over the 1 Gbps+ Leopard target
/// (5K@240 with margin), echoed in `Welcome::bitrate_kbps`. The encoder is
/// pixel-rate bound (~1 Gpix/s per NVENC, ~2 with a 2-way split), so the real
/// ceiling is the transport send path, not this number. The floor lives with
/// the derivation it floors ([`MIN_BITRATE_KBPS`]).
const MAX_BITRATE_KBPS: u32 = 8_000_000;

/// A rate this session's encoder was seen to refuse, and the clock that tests
/// that refusal again.
///
/// One transient short apply used to cap the session for good. This is the
/// client's own learned-cap lifecycle on the host side: cleared when the
/// encoder opens at a different configuration, and re-tested once the wait has
/// run out — the wait doubles each time the refusal is still there, so an
/// encoder that means it costs one ask every few minutes.
pub(super) struct EncoderCeiling {
    cap: punktfunk_core::abr::LearnedCap,
    /// When the cap was last written. The wait is `reprobe_after` report
    /// windows of real time, the same 12 s → 96 s ladder the client re-probes
    /// its own caps on.
    written_at: std::time::Instant,
}

impl EncoderCeiling {
    pub(super) fn new() -> Self {
        EncoderCeiling {
            cap: punktfunk_core::abr::LearnedCap::new(),
            written_at: std::time::Instant::now(),
        }
    }

    /// The rate to hand the encoder for an ask of `want`, and what the client
    /// is told held it there.
    ///
    /// Past the wait the ask goes through: the encoder's answer is the only
    /// evidence that the ceiling still stands. The cap moves up an eighth
    /// first, exactly as the client's re-probe does, so a refusal that is still
    /// there re-latches under the lift and backs the clock off.
    pub(super) fn resolve(&mut self, want: u32) -> (u32, AckReason) {
        let Some(cap) = self.cap.kbps() else {
            return (want, AckReason::Granted);
        };
        if want <= cap {
            return (want, AckReason::Granted);
        }
        if self.written_at.elapsed() < self.wait() {
            tracing::info!(
                requested_kbps = want,
                ceiling_kbps = cap,
                "bitrate request clamped to the known encoder ceiling"
            );
            return (cap, AckReason::EncoderLimit);
        }
        self.write(cap.saturating_add(cap / 8));
        tracing::info!(
            requested_kbps = want,
            ceiling_kbps = cap,
            "re-testing the encoder ceiling — letting the request reach the encoder"
        );
        (want, AckReason::Granted)
    }

    /// What the encoder made of an ask of `want`. Short is the ceiling, again;
    /// taking the whole ask is the ceiling gone.
    pub(super) fn note_applied(&mut self, want: u32, applied: u32) {
        if applied >= want {
            if self.cap.kbps().is_some() {
                tracing::info!(
                    applied_kbps = applied,
                    "the encoder took the whole rate — dropping the ceiling it refused before"
                );
                self.cap.drop_cap();
            }
            return;
        }
        // `latch` backs the clock off only for a cap that binds tighter; an
        // encoder that took more than the ceiling remembered has moved it up,
        // and that is not evidence of a standing refusal.
        if !self.cap.latch(applied, MIN_BITRATE_KBPS) {
            self.cap.park(applied);
        }
        self.written_at = std::time::Instant::now();
        tracing::info!(
            requested_kbps = want,
            ceiling_kbps = self.cap.kbps().unwrap_or(applied),
            retest_in_s = self.wait().as_secs(),
            "the encoder applied less than the rate asked — ceiling learned"
        );
    }

    /// The encoder opened at a different configuration. Whatever it refused was
    /// refused by an encoder that no longer exists.
    pub(super) fn clear(&mut self) {
        if self.cap.kbps().is_some() {
            tracing::info!("encoder rebuilt at a new configuration — its learned ceiling is gone");
            self.cap.drop_cap();
        }
    }

    fn write(&mut self, kbps: u32) {
        self.cap.park(kbps);
        self.written_at = std::time::Instant::now();
    }

    fn wait(&self) -> std::time::Duration {
        punktfunk_core::abr::WINDOW * self.cap.reprobe_after()
    }

    /// Spend the whole wait at once, so a test is not twelve seconds long.
    #[cfg(test)]
    fn spend_the_wait(&mut self) {
        self.written_at = self
            .written_at
            .checked_sub(self.wait())
            .expect("a monotonic clock older than one wait");
    }
}

/// `0` → host default; anything else clamped into `[MIN, MAX]`.
fn resolve_bitrate_kbps(requested: u32) -> u32 {
    if requested == 0 {
        DEFAULT_BITRATE_KBPS
    } else {
        requested.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS)
    }
}

/// PyroWave Automatic (`0`) pins ~1.6 bpp for the negotiated mode, not the 20 Mbps H.26x
/// default. ABR stays off; mid-stream retargets are refused. An explicit client rate is
/// ignored — kbps is ill-defined for all-intra bpp; every rate still goes through
/// `PUNKTFUNK_PYROWAVE_MAX_MBPS`. H.26x/AV1 explicit rates stand.
fn resolve_bitrate_kbps_for(
    codec: crate::encode::Codec,
    requested: u32,
    mode: &punktfunk_core::config::Mode,
    chroma: crate::encode::ChromaFormat,
    bit_depth: u8,
) -> u32 {
    if codec == crate::encode::Codec::PyroWave {
        if requested != 0 {
            tracing::warn!(
                requested_kbps = requested,
                "an explicit bitrate is ill-defined under PyroWave (all-intra bpp semantics) — \
                 treating it as Automatic and resolving the per-mode pin"
            );
        }
        // ~1.6 bpp 4:2:0. 4:4:4 is ×1.625 ≈ 2.6 bpp (chroma compresses better than luma);
        // 10-bit planes add ~15 %. See `design/pyrowave-444-hdr.md`.
        let bpp_x10: u64 = if chroma.is_444() { 26 } else { 16 };
        let mut bps =
            mode.width as u64 * mode.height as u64 * u64::from(mode.refresh_hz.max(1)) * bpp_x10
                / 10;
        if bit_depth >= 10 {
            bps = bps * 115 / 100;
        }
        let pin = u32::try_from(bps / 1000)
            .unwrap_or(MAX_BITRATE_KBPS)
            .clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS);
        // Open-loop pin can outrun the link. `PUNKTFUNK_PYROWAVE_MAX_MBPS` caps it;
        // unset ⇒ no cap.
        if let Some(ceiling) = pyrowave_auto_pin_ceiling_kbps() {
            if pin > ceiling {
                tracing::warn!(
                    pin_kbps = pin,
                    ceiling_kbps = ceiling,
                    "PyroWave Automatic bitrate pin exceeds PUNKTFUNK_PYROWAVE_MAX_MBPS — capping \
                     to the link ceiling (set an explicit client bitrate to choose your own)"
                );
                return ceiling.max(MIN_BITRATE_KBPS);
            }
        }
        return pin;
    }
    resolve_bitrate_kbps(requested)
}

/// Budget↔encoder at one moment: session constants plus a snapshot of adaptive FEC,
/// taken at each encoder touch. Stream loop re-derives when the live percent moves.
#[derive(Clone, Copy, Debug)]
struct EncDerive {
    audio_kbps: u32,
    shard_payload: u16,
    fec_percent: u8,
    /// PyroWave: pin is an encoder rate; both directions are identity.
    identity: bool,
}

impl EncDerive {
    fn enc_kbps(&self, budget_kbps: u32) -> u32 {
        if self.identity {
            budget_kbps
        } else {
            encoder_kbps_for_budget(
                budget_kbps,
                self.audio_kbps,
                self.fec_percent,
                self.shard_payload,
            )
        }
    }

    fn budget_kbps(&self, encoder_kbps: u32) -> u32 {
        if self.identity {
            encoder_kbps
        } else {
            budget_kbps_for_encoder(
                encoder_kbps,
                self.audio_kbps,
                self.fec_percent,
                self.shard_payload,
            )
        }
    }

    /// Read-back in the request's truncated terms. The roundtrip deflates, so a read-back
    /// that lost only truncation is the full ask. Only a genuine driver short-apply reports
    /// short.
    fn applied_budget_kbps(&self, requested_budget_kbps: u32, applied_enc_kbps: u32) -> u32 {
        let b = self.budget_kbps(applied_enc_kbps);
        if b >= self.budget_kbps(self.enc_kbps(requested_budget_kbps)) {
            requested_budget_kbps
        } else {
            b
        }
    }
}

/// Audio reservation from the resolved Welcome: PCM cost, else the same
/// [`plan_audio_budget`](punktfunk_core::audio::plan_audio_budget) rung the audio thread
/// runs, with redundancy only when `HOST_CAP_AUDIO_RED` was granted.
fn audio_reserved_kbps(welcome: &punktfunk_core::quic::Welcome) -> u32 {
    if welcome.audio_codec == punktfunk_core::quic::AUDIO_CODEC_PCM {
        punktfunk_core::audio::pcm::bitrate_kbps(
            welcome.audio_rate_hz,
            welcome.audio_bits,
            welcome.audio_channels,
        )
    } else {
        punktfunk_core::audio::plan_audio_budget(
            welcome.bitrate_kbps,
            welcome.audio_channels,
            punktfunk_core::audio::AudioLayout::from_wire(welcome.audio_layout).unwrap_or_default(),
            punktfunk_core::audio::AudioTier::default(),
            welcome.host_caps & punktfunk_core::quic::HOST_CAP_AUDIO_RED != 0,
        )
        .kbps
    }
}

/// `PUNKTFUNK_PYROWAVE_MAX_MBPS` (Mb/s) → kbps. `None` when unset/zero/invalid (no cap).
/// Every PyroWave session, including an explicit client rate, goes through the pin.
fn pyrowave_auto_pin_ceiling_kbps() -> Option<u32> {
    pf_host_config::knob("PUNKTFUNK_PYROWAVE_MAX_MBPS")
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&m| m > 0)
        .map(|m| m.saturating_mul(1000))
}

/// 2 / 6 / 8; anything else (older client, garbage) becomes stereo. Both backends can
/// produce the count; fewer real sink channels just carry up/downmixed content.
fn resolve_audio_channels(requested: u8) -> u8 {
    punktfunk_core::audio::normalize_channels(requested)
}

/// `PUNKTFUNK_FEC_PCT` pins recovery and disables adaptive FEC. `None` ⇒ adaptive. `0`
/// disables FEC. Clamped to ≤ 90.
fn fec_static_override() -> Option<u8> {
    std::env::var("PUNKTFUNK_FEC_PCT")
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .map(|p| p.min(90))
}

/// Consecutive report windows an RFI ask landed in — frames parity could not repair. The
/// client sends no [`LossReport`] for a window it discards (probe tail, host pipeline gap),
/// so a report a window late says the asks before it belong to a window nobody may price.
#[derive(Default)]
struct UnrecoveredRun {
    asked: bool,
    last_report: Option<std::time::Instant>,
    run: u32,
}

impl UnrecoveredRun {
    fn rfi(&mut self) {
        self.asked = true;
    }

    /// Close the window this report ends; returns the run it leaves.
    fn report(&mut self, now: std::time::Instant) -> u32 {
        let late = punktfunk_core::client::ADAPT_REPORT_INTERVAL * 3 / 2;
        let discarded = self
            .last_report
            .is_some_and(|t| now.duration_since(t) > late);
        self.last_report = Some(now);
        self.run = if std::mem::take(&mut self.asked) && !discarded {
            self.run.saturating_add(1)
        } else {
            0
        };
        self.run
    }
}

/// Per-frame send path: apply the adaptive-FEC target if it changed (relaxed load + compare).
fn apply_fec_target(session: &mut Session, fec_target: &AtomicU8) {
    let t = fec_target.load(Ordering::Relaxed);
    if session.fec_percent() != t {
        session.set_fec_percent(t);
    }
}

/// Host-lifetime PipeWire capturer, reused across sessions (one connect/negotiate, not per session).
type AudioCapSlot = Arc<std::sync::Mutex<Option<Box<dyn crate::audio::AudioCapturer>>>>;

/// Park an unpaired knock for console Approve. QUIC keep-alive (4 s, under 8 s idle) holds
/// the path; approval streams with no reconnect. Under the pending TTL (10 min).
const PENDING_APPROVAL_WAIT: std::time::Duration = std::time::Duration::from_secs(180);

/// A QUIC handshake that closes code 0 with no control stream is a reachability probe
/// (`--reachable` / hosts-page pips). Log at debug, not warn.
pub(crate) enum Served {
    Session,
    ProbeClose,
}

/// Handshake → input/audio → data plane. RAII teardown. A first-message PairRequest is
/// the pairing ceremony instead.
// Distinct host-lifetime handles from `serve`; a context struct would hide the lifetimes.
#[allow(clippy::too_many_arguments)]
/// The sentence a person reads when setup fails, or `None` where the host has no
/// wording better than the client's own generic one. Every close that carries text
/// goes through here: the reason bytes reach a user, and an `anyhow` chain is
/// operator register.
///
/// `downcast_ref` walks the context chain, so a failure keeps its sentence however
/// deep under `.context()` it was raised.
pub(crate) fn setup_failed_sentence(e: &anyhow::Error) -> Option<String> {
    e.downcast_ref::<pf_vdisplay::monitors::MonitorNotFound>()
        .map(|m| m.user_message())
}

// One session's whole context, threaded down rather than bundled: every argument is owned by a
// different part of the host and none of them share a lifetime.
#[allow(clippy::too_many_arguments)]
async fn serve_session(
    conn: link::SessionLink,
    opts: &Arc<Punktfunk1Options>,
    audio_cap: &AudioCapSlot,
    inj_tx: std::sync::mpsc::Sender<InputEvent>,
    mic_tx: std::sync::mpsc::SyncSender<crate::audio::MicFrame>,
    host_fp: &[u8; 32],
    np_arc: &Arc<NativePairing>,
    last_pairing: &std::sync::Mutex<Option<std::time::Instant>>,
    stats: Arc<StatsRecorder>,
    // Owned here: an unpaired knock releases it while parked and re-acquires on approval.
    mut permit: tokio::sync::OwnedSemaphorePermit,
    sem: Arc<tokio::sync::Semaphore>,
) -> Result<Served> {
    let np: &NativePairing = np_arc;
    let peer = conn.remote_address();
    let (send, mut recv) = match tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| anyhow!("control stream timeout"))??
    {
        // Clean close before any control stream: reachability probe ([`Served::ProbeClose`]).
        link::Accepted::ProbeClose => return Ok(Served::ProbeClose),
        link::Accepted::Stream(send, recv) => (send, recv),
    };
    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, io::read_msg(&mut recv))
        .await
        .map_err(|_| anyhow!("first message timeout"))??;
    if let Ok(req) = PairRequest::decode(&first) {
        // Fingerprint-bound PIN window: only this device may consume (or burn) it.
        let Some(client_fp) = conn.peer_fingerprint() else {
            close_rejected(
                &conn,
                punktfunk_core::reject::RejectReason::IdentityRequired,
            );
            anyhow::bail!("pairing requires the client to present a certificate");
        };
        let client_fp_hex = fingerprint_hex(&client_fp);
        // Charge the cooldown before consulting arming, on every outcome including rejections.
        // Otherwise "is pairing armed?" is a free oracle. A spam of knocks can hold the
        // cooldown against the real device.
        {
            let mut last = last_pairing.lock().unwrap();
            if let Some(t) = *last {
                if t.elapsed() < PAIRING_COOLDOWN {
                    close_rejected(
                        &conn,
                        punktfunk_core::reject::RejectReason::PairingRateLimited,
                    );
                    anyhow::bail!("pairing rate-limited — retry shortly");
                }
            }
            *last = Some(std::time::Instant::now());
        }
        // Live PIN per attempt so a lapsed window no longer pairs; honor fingerprint binding
        // and the address the knock came from.
        let source = crate::native_pairing::classify_source(Some(peer.ip()));
        let pin = match np.pin_for_attempt(&client_fp_hex, source) {
            crate::native_pairing::PinAttempt::Pin(pin) => pin,
            crate::native_pairing::PinAttempt::Disarmed => {
                close_rejected(&conn, punktfunk_core::reject::RejectReason::PairingNotArmed);
                anyhow::bail!(
                    "pairing not armed (arm it in the console, or start with --allow-pairing)"
                )
            }
            // Armed for a different device: reject without the ceremony so this does not burn the window.
            crate::native_pairing::PinAttempt::BoundToOther => {
                close_rejected(
                    &conn,
                    punktfunk_core::reject::RejectReason::PairingBoundToOtherDevice,
                );
                anyhow::bail!(
                    "pairing is armed for a different device — this attempt does not consume the window"
                )
            }
            // An open window is for the device in the operator's hands. This one is on the
            // internet, so it reads as not armed and the window survives for its owner.
            crate::native_pairing::PinAttempt::UnboundForWan => {
                close_rejected(&conn, punktfunk_core::reject::RejectReason::PairingNotArmed);
                anyhow::bail!(
                    "a knock from {peer} needs a pairing window bound to its fingerprint \
                     ({client_fp_hex}) — an open window does not answer the internet"
                )
            }
        };
        return pair_ceremony(&conn, send, recv, req, &client_fp, host_fp, np, &pin)
            .await
            .map(|()| Served::Session);
    }

    // Pairing gate outside the handshake future: approval wait must not be bound by
    // HANDSHAKE_TIMEOUT, and the NVENC permit is released while parked.
    if opts.require_pairing {
        // Hello name for the pending label; handshake re-decodes for the real session.
        let gate_hello = Hello::decode(&first).map_err(|e| anyhow!("Hello decode: {e:?}"))?;
        if gate_hello.abi_version != punktfunk_core::WIRE_VERSION {
            close_rejected(
                &conn,
                punktfunk_core::reject::RejectReason::WireVersionMismatch,
            );
            anyhow::bail!(
                "wire version mismatch: client {} host {}",
                gate_hello.abi_version,
                punktfunk_core::WIRE_VERSION
            );
        }
        let fp = conn.peer_fingerprint();
        // `effective`, not `is_paired`: an expired record is listed but not authorized, so it
        // knocks like an unpaired device and re-approval is the re-grant.
        let authorized = fp
            .as_ref()
            .map(|fp| {
                np.effective(&fingerprint_hex(fp), wall_unix_now())
                    .is_some()
            })
            .unwrap_or(false);
        if !authorized {
            // Anonymous: no identity to approve. PIN ceremony is the way in.
            let Some(fp) = fp else {
                close_rejected(
                    &conn,
                    punktfunk_core::reject::RejectReason::IdentityRequired,
                );
                anyhow::bail!(
                    "unpaired anonymous client rejected (this host requires pairing — present a \
                     client identity and approve it in the console, or run the PIN ceremony)"
                );
            };
            let fp_hex = fingerprint_hex(&fp);
            // Sanitize the wire name before log/console (escapes / bidi). Empty → fingerprint label.
            let label = crate::native_pairing::sanitize_device_name(
                gate_hello.name.as_deref().unwrap_or(""),
                &fp_hex,
            );
            tracing::info!(name = %label, fingerprint = %fp_hex,
                "unpaired device knocked — parking connection for delegated approval in the console");
            // QUIC-validated source IP for the pending per-source cap. Knock generation makes
            // this connection the one an approval admits — siblings must not all start a session.
            let knock_seq = np.note_pending(&label, &fp_hex, Some(peer.ip()));
            // Parked knock must not hold an NVENC permit.
            drop(permit);
            let decision = tokio::select! {
                d = np.wait_for_decision(&fp_hex, knock_seq, PENDING_APPROVAL_WAIT) => d,
                _ = conn.closed() => anyhow::bail!("client disconnected before pairing approval"),
            };
            match decision {
                PairingDecision::Approved => {
                    tracing::info!(name = %label, fingerprint = %fp_hex,
                        "device approved in console — admitting session (no reconnect)");
                }
                PairingDecision::Denied => {
                    close_rejected(&conn, punktfunk_core::reject::RejectReason::Denied);
                    anyhow::bail!("pairing request denied in the console")
                }
                PairingDecision::TimedOut => {
                    close_rejected(&conn, punktfunk_core::reject::RejectReason::ApprovalTimeout);
                    anyhow::bail!(
                        "pairing request not approved within {PENDING_APPROVAL_WAIT:?} \
                         — the device can knock again"
                    )
                }
                PairingDecision::Superseded => {
                    close_rejected(&conn, punktfunk_core::reject::RejectReason::Superseded);
                    anyhow::bail!(
                        "parked knock superseded by a newer connection from the same device — \
                         only the newest is admitted on approval"
                    )
                }
            }
            // Re-acquire like any freshly accepted client (waits if busy).
            permit = sem
                .clone()
                .acquire_owned()
                .await
                .expect("session semaphore is never closed");
        }
    }
    // Admitted. From here the session is the same on every carrier; the certificate's
    // fingerprint is what this plane admitted by, and the browser plane passes its own.
    let session_fp_hex = conn.peer_fingerprint().map(|fp| fingerprint_hex(&fp));
    let host = SessionHost {
        opts: Arc::clone(opts),
        audio_cap: audio_cap.clone(),
        inj_tx,
        mic_tx,
        np: np_arc.clone(),
        stats,
    };
    run_admitted(
        conn,
        send,
        recv,
        first,
        session_fp_hex,
        &host,
        DataPlane::Udp,
        permit,
    )
    .await
}

/// Everything a session needs from the plane that admitted it. One value, cloned per session,
/// because the browser plane admits differently (a device key, not a certificate) and then runs
/// exactly this session — and a second copy of the runner is the thing this must never become.
#[derive(Clone)]
pub(crate) struct SessionHost {
    pub(crate) opts: Arc<Punktfunk1Options>,
    pub(crate) audio_cap: AudioCapSlot,
    pub(crate) inj_tx: std::sync::mpsc::Sender<InputEvent>,
    pub(crate) mic_tx: std::sync::mpsc::SyncSender<crate::audio::MicFrame>,
    pub(crate) np: Arc<NativePairing>,
    pub(crate) stats: Arc<StatsRecorder>,
}

impl SessionHost {
    /// A host with nothing behind it: every channel's receiver is dropped. For tests of the
    /// admission code, which never reach the pipeline.
    #[cfg(test)]
    pub(crate) fn for_tests(np: Arc<NativePairing>) -> SessionHost {
        SessionHost {
            opts: Arc::new(Punktfunk1Options {
                port: 0,
                source: Punktfunk1Source::Synthetic,
                seconds: 0,
                frames: 0,
                max_sessions: 0,
                max_concurrent: 1,
                require_pairing: true,
                allow_pairing: false,
                pairing_pin: None,
                paired_store: None,
                data_port: None,
                idle_timeout: None,
                mdns: false,
            }),
            audio_cap: Arc::new(std::sync::Mutex::new(None)),
            inj_tx: std::sync::mpsc::channel().0,
            mic_tx: std::sync::mpsc::sync_channel(1).0,
            np,
            stats: StatsRecorder::new(crate::stats_recorder::default_dir()),
        }
    }
}

/// Where video goes.
///
/// The native plane binds a UDP socket during the handshake and hole-punches to the client; a
/// browser has no second socket, so its video rides the same WebTransport connection as
/// everything else. The stream thread turns either into the `Box<dyn Transport>` that
/// `Session` was always written against, which is why nothing below it knows the difference.
pub(crate) enum DataPlane {
    Udp,
    Web(crate::webtransport::WebTransportPlane),
}

/// The session proper, after admission. Carrier-agnostic: the control stream is a [`link::CtlSend`]
/// / [`link::CtlRecv`] pair, datagrams go through [`link::SessionLink`], and video through
/// whatever [`DataPlane`] the caller built.
#[allow(clippy::too_many_arguments)] // one value per thing the two admission paths resolve
pub(crate) async fn run_admitted(
    conn: link::SessionLink,
    send: link::CtlSend,
    recv: link::CtlRecv,
    first: Vec<u8>,
    session_fp_hex: Option<String>,
    host: &SessionHost,
    data_plane: DataPlane,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Served> {
    let SessionHost {
        opts,
        audio_cap,
        inj_tx,
        mic_tx,
        np,
        stats,
    } = host;
    let (opts, np, stats) = (opts.as_ref(), np.as_ref(), stats.clone());
    let (inj_tx, mic_tx) = (inj_tx.clone(), mic_tx.clone());
    let (mut send, mut recv) = (send, recv);
    let peer = conn.remote_address();
    // RAII frees the slot on return.
    let _permit = permit;

    // Grants once at admission: effective mask + deadline + watch. Anonymous (`--open`) and
    // an identity with no record keep full control — nothing on the trust record to enforce.
    let admit_unix = wall_unix_now();
    let (initial_grants, deadline_unix, access_watch) = match session_fp_hex.as_deref() {
        Some(fp_hex) => match np.effective(fp_hex, admit_unix) {
            Some(mask) => {
                // Subscribe before reading the deadline so a racing edit lands in this borrow
                // or as the first change — never in a gap.
                let rx = np.subscribe(fp_hex);
                let deadline = rx.borrow().deadline_unix;
                (mask, deadline, Some(rx))
            }
            // Expired between the pairing gate and here: typed expiry, not a setup error.
            None if opts.require_pairing => {
                close_rejected(&conn, punktfunk_core::reject::RejectReason::AccessExpired);
                anyhow::bail!("access expired between admission and session setup");
            }
            // `--open`: unpaired / expired identities keep full control.
            None => (GRANT_ALL, None, None),
        },
        None => (GRANT_ALL, None, None),
    };
    // Count this session while it runs. Held to the end of the function, so an error return or
    // a cancelled task releases it too — a record granted "this session" is dropped once the
    // guard falls and nothing reconnects inside the grace.
    let _session = session_fp_hex
        .as_deref()
        .map(|fp_hex| host.np.session_started(fp_hex));
    // One relaxed load per event; the lifecycle task is the only writer after admission.
    let session_grants = Arc::new(AtomicU32::new(initial_grants));
    // Launch without LAUNCH: refuse before handshake (typed reason), not a silent bare desktop.
    if initial_grants & GRANT_LAUNCH == 0 && Hello::decode(&first).is_ok_and(|h| h.launch.is_some())
    {
        close_rejected(
            &conn,
            punktfunk_core::reject::RejectReason::LaunchNotPermitted,
        );
        anyhow::bail!("client requested a library launch without the LAUNCH grant");
    }
    let expires_in_secs = remaining_secs_wire(deadline_unix, admit_unix);

    let source = opts.source;
    let frames = opts.frames;
    let data_port = opts.data_port;
    // Hello in hand; send thread finishes this when the first video packet leaves.
    let bringup = crate::bringup::Trace::start("bringup", Arc::new(AtomicU32::new(0)));
    // Mid-stream resize counterpart; latest accepted Reconfigure wins.
    let resize_ms: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));

    // Created before handshake so Welcome-time display prep aborts if the client vanishes.
    let stop = Arc::new(AtomicBool::new(false));
    // Set before `stop` on `QUIT_CODE` so the display lease skips the keep-alive linger.
    let quit = Arc::new(AtomicBool::new(false));
    // Why the session ended, for its summary. Latched first-writer-wins, so the host's own
    // close (game exit, clean finish) beats the `Other` this watcher would read it back as.
    let end_reason = Arc::new(std::sync::atomic::AtomicU8::new(0));
    // Session totals for the summary. The input, audio and encode paths all outlive the
    // guard that reads them, so they bump a shared block rather than hand a figure over.
    let counters = Arc::new(crate::session_status::SessionCounters::default());
    {
        let stop = stop.clone();
        let quit = quit.clone();
        let end_reason = end_reason.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            let reason = conn.closed().await;
            if reason.closed_with(QUIT_CODE) {
                quit.store(true, Ordering::SeqCst);
                crate::events::SessionEndReason::Local.latch(&end_reason);
            } else {
                // The client's own rule: anything that is not our close code is the link
                // going away. A close this host made reads as `Other` here too, which is
                // why the paths that make one latch before they call it.
                crate::events::SessionEndReason::Lost.latch(&end_reason);
            }
            stop.store(true, Ordering::SeqCst);
        });
    }

    let (
        hello,
        welcome,
        udp_port,
        data_sock,
        start,
        client_label,
        abr_features,
        compositor,
        gamescope_route,
        prep,
        joined,
    ) = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        handshake::negotiate(
            &conn,
            &mut send,
            &mut recv,
            &first,
            source,
            frames,
            // No UDP socket for a browser: its video rides the connection it is already on.
            matches!(data_plane, DataPlane::Udp).then_some(data_port),
            &bringup,
            quit.clone(),
            stop.clone(),
            initial_grants,
            expires_in_secs,
        ),
    )
    .await
    .map_err(|_| anyhow!("handshake timed out after {HANDSHAKE_TIMEOUT:?}"))??;
    let (ctrl_send, ctrl_recv) = (send, recv);
    let join_live = joined.is_some();
    let reframe_to = joined.as_ref().map(|(_, view)| {
        (
            punktfunk_core::video_fit::VideoFit::from_wire(hello.video_fit),
            *view,
        )
    });
    // Filled by the stream thread's encoder open; the input thread reads it.
    let frame_map = input::FrameMap::default();
    let gamescope_hold =
        (compositor == Some(crate::vdisplay::Compositor::Gamescope)).then(GamescopeHold::new);
    // Live reconfigure is off for gamescope (resize must not relaunch the title),
    // `identity: per-client-mode` (resize would resolve a different slot), a monitor
    // mirror (physical head ignores the requested mode) and a `join` session (the mode
    // is the owner's). Synthetic stays on. Captured once here.
    let live_reconfig_ok = {
        let per_client_mode_identity = crate::vdisplay::policy::prefs()
            .configured_effective()
            .is_some_and(|e| e.identity == crate::vdisplay::policy::Identity::PerClientMode);
        // Pin at bring-up; a console change mid-session must not change this session's answer.
        // Linux-only: `vdisplay::open` only routes to the mirror there.
        #[cfg(target_os = "linux")]
        let mirrored = crate::vdisplay::capture_monitor().is_some();
        #[cfg(not(target_os = "linux"))]
        let mirrored = false;
        reconfig_allowed(compositor, per_client_mode_identity, mirrored || join_live)
    };
    // `Copy` so the control task's `async move` and SessionContext both keep it.
    let codec = crate::encode::codec_from_wire(welcome.codec);
    let client_udp = std::net::SocketAddr::new(peer.ip(), start.client_udp_port);
    tracing::info!(
        %client_udp,
        udp_port,
        mode = ?hello.mode,
        compositor = compositor.map(|c| c.id()).unwrap_or("none"),
        gamepad = welcome.gamepad.as_str(),
        // Build + the shell that dialled, so two sessions from one device are told apart here.
        // "-" is a client too old to send one.
        client = client_label.as_deref().unwrap_or("-"),
        "handshake complete — streaming"
    );

    // Handshake stream stays open: Reconfigure → data plane rebuilds capture/encoder;
    // ProbeRequest → FLAG_PROBE burst. Inbound and outbound multiplexed with `select!`.
    let (reconfig_tx, reconfig_rx) = std::sync::mpsc::channel::<punktfunk_core::Mode>();
    let (keyframe_tx, keyframe_rx) = std::sync::mpsc::channel::<()>();
    // LTR-RFI: encode loop prefers `invalidate_ref_frames` over a full IDR when the encoder can.
    let (rfi_tx, rfi_rx) = std::sync::mpsc::channel::<(u32, u32)>();
    let (bitrate_tx, bitrate_rx) = std::sync::mpsc::channel::<u32>();
    // Encoder truth for `SetBitrate` resolve: applied rate, a ceiling the encoder taught and
    // re-tests, cadence-degraded (climb refused — more bits are not the fix).
    let live_bitrate = Arc::new(AtomicU32::new(welcome.bitrate_kbps));
    let encoder_ceiling = Arc::new(std::sync::Mutex::new(EncoderCeiling::new()));
    let cadence_degraded = Arc::new(AtomicBool::new(false));
    // Behind-cadence score for the climb-refusal log (the flag alone has no evidence).
    let cadence_behind_score = Arc::new(AtomicU32::new(0));
    // Client-received packet count (`u32::MAX` until the client answers). Distinguishes a
    // clean link from a dead one (`loss_ppm = 0` means both).
    let client_packets_received = Arc::new(AtomicU32::new(u32::MAX));
    let client_packets_received_ctl = client_packets_received.clone();
    let (probe_tx, probe_rx) = std::sync::mpsc::channel::<ProbeRequest>();
    let (probe_result_tx, probe_result_rx) = tokio::sync::mpsc::unbounded_channel::<ProbeResult>();
    // Accept ack is written before the rebuild; a failed or differently-honored rebuild must
    // correct the client's mode slot with a second `Reconfigured { accepted: true, mode }`.
    let (reconfig_result_tx, reconfig_result_rx) =
        tokio::sync::mpsc::unbounded_channel::<Reconfigured>();
    // Rebuild can re-resolve Automatic (1080p client mirroring a 4K panel). Tell the client
    // (`BitrateChanged`); otherwise ABR's first climb is from a stale lower base.
    let (retarget_tx, retarget_rx) = tokio::sync::mpsc::unbounded_channel::<(u32, AckReason)>();
    // Rebuild gap (ms) → `PipelineGap` so the client discards that ABR window as congestion.
    let (gap_tx, gap_rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
    // Encode loop diffs cursor serial; control task is the sole writer. Wired even if unused.
    // Depth-1 latest-wins: a shape a stalled peer never drained is stale, not a backlog.
    let (cursor_shape_tx, cursor_shape_rx) =
        tokio::sync::watch::channel::<Option<punktfunk_core::quic::CursorShape>>(None);
    // Channels always wired. Driver only if `Hello::max_shard_payload` and not PyroWave
    // (PyroWave pins the Welcome value for the session; mid-stream re-key would desync).
    let (shard_change_tx, shard_change_rx) = tokio::sync::mpsc::unbounded_channel::<u16>();
    let (shard_ack_tx, shard_ack_rx) = tokio::sync::mpsc::unbounded_channel::<u16>();
    let (shard_apply_tx, shard_apply_rx) = std::sync::mpsc::channel::<usize>();
    // Not for a browser either: the driver's targets are UDP-over-IP maths, and a WebTransport
    // datagram also carries QUIC and HTTP/3 framing, so a grow it computed would not fit.
    let shard_reneg = (hello.max_shard_payload > 0
        && codec != crate::encode::Codec::PyroWave
        && matches!(data_plane, DataPlane::Udp))
    .then_some(wire_mtu::ShardReneg {
        client_ceiling: hello.max_shard_payload,
        change_tx: shard_change_tx,
        ack_rx: shard_ack_rx,
        apply_tx: shard_apply_tx,
    });
    // Path-MTU watch: clamp for the next session, and heal/grow this one if the driver exists.
    wire_mtu::spawn_watch(
        conn.clone(),
        welcome.shard_payload as usize,
        hello.max_shard_payload,
        shard_reneg,
    );
    // Read back from Welcome, not recomputed (would re-probe and could drift).
    let cursor_forward = welcome.host_caps & punktfunk_core::quic::HOST_CAP_CURSOR != 0;
    // `true` = client draws (exclude + forward), `false` = host composites. Starts true.
    let cursor_client_draws = Arc::new(AtomicBool::new(true));
    let cursor_client_draws_dp = cursor_client_draws.clone();
    // Control task publishes LossReport → recovery %; send loop applies per frame. Seeded no-op.
    let adaptive_fec = fec_static_override().is_none();
    let fec_target = Arc::new(AtomicU8::new(welcome.fec.fec_percent));
    let fec_target_ctl = fec_target.clone();
    // PhaseReports from the control task; encode loop drains. Inert until a vsync-aware client.
    let phase_ctl = Arc::new(stream::PhaseCtl::new());
    let phase_ctl_control = phase_ctl.clone();
    // Negotiated rate; PyroWave retarget-refusals ack this pin.
    let session_bitrate_kbps = welcome.bitrate_kbps;
    // Control task flips on `ClipControl`; lifecycle clears it if CLIPBOARD is revoked.
    let clip_enabled = Arc::new(AtomicBool::new(false));
    // Without CLIPBOARD the coordinator never starts (a watcher that doesn't exist can't leak).
    // Inert handle (`available: false`) keeps control-task arms uniform: NOT_PERMITTED, and
    // the decline loop still answers stray fetches.
    // The clipboard's fetch transfers are quinn streams, so it is on offer only where there are
    // some — a browser takes the declining arm below until the control plane is carrier-agnostic.
    let clip_quic = (initial_grants & GRANT_CLIPBOARD != 0)
        .then(|| conn.as_quic().cloned())
        .flatten();
    let clip = if let Some(quic) = clip_quic {
        pf_clipboard::start(quic, clip_enabled.clone(), compositor.is_some()).await
    } else {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_offer_tx, offer_rx) = tokio::sync::mpsc::unbounded_channel();
        pf_clipboard::ClipCoord {
            available: false,
            cmd_tx,
            offer_rx,
        }
    };
    let clip_available = clip.available;
    // Lifecycle and the per-session management routes → control task. Both lanes stay
    // open for the whole session: the console can re-point or mute an anonymous client too.
    let (access_tx, access_rx) = tokio::sync::mpsc::unbounded_channel::<AccessUpdate>();
    let (audio_tx, audio_rx) =
        tokio::sync::mpsc::unbounded_channel::<punktfunk_core::quic::AudioState>();
    // Input thread → client: which player each of this session's pads is.
    let (pad_slots_tx, pad_slots_rx) =
        tokio::sync::mpsc::unbounded_channel::<punktfunk_core::quic::PadSlots>();
    // The device's stored player pick. Keyed by the pairing fingerprint, never by
    // an address: the same device reconnecting is the same player.
    let preferred_pad_slot = session_fp_hex.as_deref().and_then(|fp| np.pad_slot_of(fp));
    let pad_id =
        crate::inject::pad_pool::PadIdentity::new(session_fp_hex.as_deref(), preferred_pad_slot);
    let pad_slots = Arc::new(std::sync::atomic::AtomicU16::new(0));
    // Launch verdict lane. Unbounded and opened here so the library resolve below
    // can refuse onto it before the stream thread exists.
    let (launch_outcome_tx, launch_outcome_rx) =
        tokio::sync::mpsc::unbounded_channel::<punktfunk_core::quic::LaunchOutcome>();
    let launch_outcome_dp = launch_outcome_tx.clone();
    // What `DELETE /session/{id}` and its siblings act on. `ceiling` is the pairing's own
    // mask: a live re-point clamps to it, so the console never grants past the pairing.
    let controls = crate::session_status::SessionControls {
        grants: session_grants.clone(),
        ceiling: Arc::new(AtomicU32::new(initial_grants)),
        muted: Arc::new(AtomicBool::new(false)),
        deadline_unix: Arc::new(std::sync::atomic::AtomicI64::new(
            deadline_unix.unwrap_or(0),
        )),
        access_tx: Some(access_tx.clone()),
        audio_tx: Some(audio_tx),
        // Filled by the stream thread once capture names the head.
        head: Arc::new(std::sync::Mutex::new(None)),
        pad_slots: pad_slots.clone(),
        fingerprint: session_fp_hex.clone(),
        pad_owner: pad_id.owner,
        preferred_pad_slot: Arc::new(std::sync::atomic::AtomicU8::new(
            preferred_pad_slot.unwrap_or(crate::session_status::NO_PAD_SLOT),
        )),
        // Written by the input thread below, read by `GET /session/{id}/pads`.
        pads: Arc::new(crate::pad_feed::PadFeed::new()),
    };
    tokio::spawn(control::run(control::Task {
        ctrl_send,
        ctrl_recv,
        initial_mode: hello.mode,
        codec,
        live_reconfig_ok,
        adaptive_fec,
        session_bitrate_kbps,
        ack_reason: abr_features & punktfunk_core::quic::EXT_ABR_ACK_REASON != 0,
        live_bitrate: live_bitrate.clone(),
        encoder_ceiling: encoder_ceiling.clone(),
        cadence_degraded: cadence_degraded.clone(),
        cadence_behind_score: cadence_behind_score.clone(),
        client_packets_received: client_packets_received_ctl,
        fec_target_ctl,
        phase_ctl: phase_ctl_control,
        reconfig_tx,
        keyframe_tx,
        rfi_tx,
        bitrate_tx,
        probe_tx,
        probe_result_rx,
        reconfig_result_rx,
        retarget_rx,
        gap_rx,
        shard_change_rx,
        shard_ack_tx,
        cursor_shape_rx,
        cursor_client_draws,
        clip_enabled: clip_enabled.clone(),
        clip,
        session_grants: session_grants.clone(),
        access_rx,
        audio_rx,
        pad_slots_rx,
        launch_outcome_rx,
        peer: peer.ip(),
        counters: counters.clone(),
        stats: stats.clone(),
    }));
    // Trust-store name (a console rename wins), else the sanitized Hello name. `None` if
    // nameless. Events, hook filters, the stream marker and the tray all show this one.
    let client_name = session_fp_hex
        .as_deref()
        .and_then(|fp| np.list().into_iter().find(|c| c.fingerprint == fp))
        .map(|c| c.name)
        .or_else(|| {
            let raw = hello.name.as_deref().unwrap_or("").trim();
            (!raw.is_empty()).then(|| {
                crate::native_pairing::sanitize_device_name(
                    raw,
                    session_fp_hex.as_deref().unwrap_or(""),
                )
            })
        });
    // Only a fingerprint has a record to watch; with no record there is nothing to expire.
    match (session_fp_hex.clone(), access_watch) {
        (Some(fp_hex), Some(watch_rx)) => {
            let device = crate::events::DeviceRef {
                name: client_name
                    .clone()
                    .unwrap_or_else(|| crate::native_pairing::sanitize_device_name("", &fp_hex)),
                fingerprint: fp_hex,
                plane: crate::events::Plane::Native,
            };
            tokio::spawn(access_lifecycle(
                conn.clone(),
                watch_rx,
                controls.clone(),
                clip_enabled.clone(),
                access_tx,
                deadline_unix,
                device,
            ));
        }
        _ => drop(access_tx),
    }
    // No backend: decline fetches instead of hanging (coordinator owns `accept_bi` when live).
    if !clip_available && pf_clipboard::enabled() {
        if let Some(quic) = conn.as_quic() {
            pf_clipboard::spawn_decline_loop(quic.clone());
        }
    }

    // Isolated gamescope: per-session input/audio/mic. Identity is the cert-fingerprint prefix
    // so keep-alive hands a kept spawn back to the same client. Minted after handshake, before
    // the input/audio threads (`compositor::session_is_isolated`).
    #[cfg(target_os = "linux")]
    let isolation: Option<crate::vdisplay::SessionIsolation> = match joined.as_ref().map(|(d, _)| d)
    {
        // A joiner uses the owner's planes: its input relay and sink. A second mic source of the
        // same name would split the owner's, so this session's mic stays on the shared one.
        Some(d) => d
            .isolation
            .clone()
            .map(|i| crate::vdisplay::SessionIsolation {
                mic_source: None,
                ..i
            }),
        None => compositor
            .filter(|c| compositor::session_is_isolated(*c, gamescope_route.as_ref()))
            .map(|_| {
                // `--open` has no fingerprint; a per-accept sequence isolates at the cost of keep-alive.
                static ANON_SEQ: AtomicU64 = AtomicU64::new(0);
                let id = session_fp_hex
                    .as_deref()
                    .map(|fp| fp[..fp.len().min(8)].to_string())
                    .unwrap_or_else(|| format!("anon{}", ANON_SEQ.fetch_add(1, Ordering::Relaxed)));
                // Monitor-mode has no per-session sink — audio stays shared; input/mic still isolate.
                let sink = crate::audio::per_session_sink_possible()
                    .then(|| format!("punktfunk-speaker-iso-{id}"));
                let mic_source = Some(format!("punktfunk-mic-{id}"));
                tracing::info!(%id, sink = sink.as_deref().unwrap_or("-"),
                "isolated gamescope session — per-session input/audio/mic planes");
                crate::vdisplay::SessionIsolation::new(id, sink, mic_source)
            }),
    };
    // Pinned injector + swappable route. Drop at session end closes the EIS connection.
    #[cfg(target_os = "linux")]
    let session_injector = isolation
        .as_ref()
        .map(|i| crate::inject::InjectorService::start_at(i.ei_relay.clone()));
    #[cfg(target_os = "linux")]
    let inj_session_tx = session_injector.as_ref().map(|s| s.sender());
    #[cfg(target_os = "linux")]
    let input_route = input::InputRoute::new(match &inj_session_tx {
        Some(tx) => tx.clone(),
        None => inj_tx.clone(),
    });
    #[cfg(not(target_os = "linux"))]
    let input_route = input::InputRoute::new(inj_tx.clone());
    // Isolated mic pump (`punktfunk-mic-{id}`) for this session's 0xCB uplink. Drop tears it down.
    #[cfg(target_os = "linux")]
    let session_mic = isolation
        .as_ref()
        .and_then(|i| i.mic_source.clone())
        .map(|name| crate::audio::MicPump::start_named(Some(name)));
    #[cfg(target_os = "linux")]
    let mic_tx = session_mic.as_ref().map(|p| p.sender()).unwrap_or(mic_tx);

    // One bounded channel for pointer/keyboard and rich input. Unbounded is RSS DoS: the
    // producer outruns the consumer; pen batches amplify. Drop is correct — stale input is
    // already worthless; the injector re-syncs from the next event.
    const INPUT_QUEUE_DEPTH: usize = 1024;
    let (input_tx, input_rx) = std::sync::mpsc::sync_channel::<ClientInput>(INPUT_QUEUE_DEPTH);
    let rich_tx = input_tx.clone();
    // Stream loop parks the seat pointer through the same path client input takes.
    #[cfg(target_os = "linux")]
    let input_tx_stream = input_tx.clone();
    let input_handle = {
        let conn = conn.clone();
        let stop = stop.clone();
        let gamepad = welcome.gamepad;
        // Read HOST_CAP_PAD_AUDIO back off Welcome so the input thread cannot disagree.
        let pad_audio_on = welcome.host_caps & punktfunk_core::quic::HOST_CAP_PAD_AUDIO != 0;
        let grants = session_grants.clone();
        let frame_map = frame_map.clone();
        let pad_feed = controls.pads.clone();
        let counters = counters.clone();
        std::thread::Builder::new()
            .name("punktfunk1-input".into())
            .spawn({
                let input_route = input_route.clone();
                move || {
                    input_thread(
                        input_rx,
                        conn,
                        input_route,
                        gamepad,
                        pad_audio_on,
                        pad_id,
                        pad_slots,
                        Some(pad_slots_tx),
                        grants,
                        frame_map,
                        pad_feed,
                        stop,
                        counters,
                    )
                }
            })
            .context("spawn input thread")?
    };
    // One `read_datagram` loop (two would race): 0xCB mic, 0xCC rich, 0xC8 input. Magics disjoint.
    let input_conn = conn.clone();
    let grants_dp = session_grants.clone();
    let counters_dp = counters.clone();
    tokio::spawn(async move {
        // Shared, not local: this task ends with the connection, which closes after the session
        // summary is built, so a local total would never reach it.
        let n = &*counters_dp;
        // Per-class counts; one warn on the first drop; totals at end-of-stream.
        let denied = GrantDrops::new();
        // Full queue: drop, never block (would stall mic + this reader). Disconnected ends the loop.
        let offer = |tx: &std::sync::mpsc::SyncSender<ClientInput>, item: ClientInput| match tx
            .try_send(item)
        {
            Ok(()) => true,
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                n.input_dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        };
        while let Ok(d) = input_conn.read_datagram().await {
            // One relaxed load per datagram; test before offering. Mic/rich/pen by plane tag;
            // 0xC8 through `classify`.
            let mask = grants_dp.load(Ordering::Relaxed);
            if let Some((seq, pts, opus)) = punktfunk_core::quic::decode_mic_datagram(&d) {
                if mask & GRANT_MIC == 0 {
                    // Dropping here is the setup gate: forwarding is the only attach this plane has.
                    denied.note(GrantClass::Mic);
                    continue;
                }
                n.input_mic.fetch_add(1, Ordering::Relaxed);
                // Bounded `try_send`: never block this loop. seq + pts ride for de-jitter.
                let _ = mic_tx.try_send(crate::audio::MicFrame {
                    seq,
                    pts_ns: pts,
                    opus: opus.to_vec(),
                });
            } else if let Some(rich) = punktfunk_core::quic::RichInput::decode(&d) {
                if mask & GRANT_GAMEPAD == 0 {
                    denied.note(GrantClass::Gamepad);
                    continue;
                }
                n.input_rich.fetch_add(1, Ordering::Relaxed);
                if !offer(&rich_tx, ClientInput::Rich(rich)) {
                    break;
                }
            } else if let Some(pen) = punktfunk_core::quic::PenBatch::decode(&d) {
                // 0xCC kind 0x05 stylus (`RichInput::decode` returns None). Same input thread.
                if mask & GRANT_POINTER == 0 {
                    denied.note(GrantClass::Pointer);
                    continue;
                }
                n.input_rich.fetch_add(1, Ordering::Relaxed);
                if !offer(&rich_tx, ClientInput::Pen(pen)) {
                    break;
                }
            } else if let Some(mut ev) = InputEvent::decode(&d) {
                let class = classify(ev.kind);
                if mask & class.bit() == 0 {
                    denied.note(class);
                    continue;
                }
                n.input_events.fetch_add(1, Ordering::Relaxed);
                // KEY_FLAG_SEMANTIC_VK is in-process (GameStream ingest). Strip it from the wire.
                if matches!(
                    ev.kind,
                    punktfunk_core::input::InputKind::KeyDown
                        | punktfunk_core::input::InputKind::KeyUp
                ) {
                    ev.flags &= !crate::inject::KEY_FLAG_SEMANTIC_VK;
                }
                if !offer(&input_tx, ClientInput::Event(ev)) {
                    break;
                }
            }
        }
        tracing::info!(
            input = n.input_events.load(Ordering::Relaxed),
            mic = n.input_mic.load(Ordering::Relaxed),
            rich = n.input_rich.load(Ordering::Relaxed),
            dropped = n.input_dropped.load(Ordering::Relaxed),
            denied = %denied.summary(),
            "client datagram stream ended"
        );
    });

    // Handshake complete: CONNECTED. A client rejected earlier never emits either.
    let event_client = crate::events::ClientRef {
        name: client_name.clone().unwrap_or_default(),
        fingerprint: session_fp_hex.clone(),
        plane: crate::events::Plane::Native,
    };
    crate::events::emit(crate::events::EventKind::ClientConnected {
        client: event_client.clone(),
    });
    {
        let conn = conn.clone();
        tokio::spawn(async move {
            let reason = conn.closed().await;
            let why = if reason.closed_with(QUIT_CODE) {
                crate::events::DisconnectReason::Quit
            } else if matches!(reason, link::LinkClosed::TimedOut) {
                crate::events::DisconnectReason::Timeout
            } else {
                crate::events::DisconnectReason::Error
            };
            crate::events::emit(crate::events::EventKind::ClientDisconnected {
                client: event_client,
                reason: why,
            });
        });
    }

    // Mode-conflict admission: later clients see this identity + mode + stop (and may `steal`).
    // The audio thread publishes the sink it captures here, for a joiner to tap.
    let audio_sink: Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let _live_guard = {
        let id = conn.peer_fingerprint();
        let label = id
            .map(|fp| {
                fp.iter()
                    .take(4)
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            })
            .unwrap_or_else(|| "client".to_string());
        crate::vdisplay::admission::register(
            id,
            (
                welcome.mode.width,
                welcome.mode.height,
                welcome.mode.refresh_hz,
            ),
            stop.clone(),
            label,
            crate::vdisplay::admission::LiveDisplay {
                compositor,
                route: gamescope_route.clone(),
                #[cfg(target_os = "linux")]
                isolation: isolation.clone(),
                #[cfg(not(target_os = "linux"))]
                isolation: None,
                audio_sink: audio_sink.clone(),
            },
        )
    };

    // Not for the byte-pattern source, which has a test client that wants nothing else on the
    // wire. Best-effort: a spawn error must not early-return (threads already up).
    let audio_handle = if opts.source != Punktfunk1Source::Synthetic {
        let conn = conn.clone();
        let stop = stop.clone();
        let cap = audio_cap.clone();
        let channels = welcome.audio_channels;
        // Format from Welcome bytes, not a second evaluation of the gate (config + live property).
        let audio_plane = handshake::AudioPlane::from_welcome(&welcome);
        // Read the granted bit back off Welcome, then re-derive the same budget rung from it.
        let budget = handshake::audio_budget(
            welcome.host_caps & punktfunk_core::quic::HOST_CAP_AUDIO_RED != 0,
            welcome.bitrate_kbps,
            channels,
            audio_plane.layout,
        );
        // Isolated session captures its own named sink; `None` is the shared path. A joiner
        // taps the owner's sink either way: its isolated one, or the one the owner published.
        #[cfg(target_os = "linux")]
        let iso_sink = isolation.as_ref().and_then(|i| i.sink.clone());
        #[cfg(not(target_os = "linux"))]
        let iso_sink: Option<String> = None;
        let tap_from = joined.as_ref().map(|(d, _)| d.audio_sink.clone());
        let published = audio_sink.clone();
        let muted = controls.muted.clone();
        let counters = counters.clone();
        std::thread::Builder::new()
            .name("punktfunk1-audio".into())
            .spawn(move || {
                audio_thread(
                    conn,
                    stop,
                    cap,
                    channels,
                    budget,
                    audio_plane,
                    iso_sink,
                    join_live,
                    published,
                    tap_from,
                    muted,
                    counters,
                )
            })
            .map_err(|e| tracing::warn!(error = %e, "audio thread spawn failed — session continues without audio"))
            .ok()
    } else {
        None
    };

    // HDR10 baseline at start. The virtual stream then sends the source's real mastering
    // (GetDesc1) on capture start and keyframes. This covers synthetic + the pre-capture gap.
    if welcome.color.is_hdr() {
        // Client display volume (Hello::display_hdr) — EDID advertises it. Generic HDR10 for old clients.
        let meta = hello
            .display_hdr
            .unwrap_or_else(|| crate::encode::hdr_meta_to_wire(pf_frame::hdr::generic_hdr10()));
        let _ = conn.send_datagram(punktfunk_core::quic::encode_hdr_meta_datagram(&meta));
        tracing::info!(
            client_volume = hello.display_hdr.is_some(),
            "sent HDR10 static metadata (0xCE baseline)"
        );
    }

    // Synthetic-only test hook: rumble (0xCA) + HID-output (0xCD) for loopback, no real pad.
    if opts.source == Punktfunk1Source::Synthetic
        && std::env::var("PUNKTFUNK_TEST_FEEDBACK").as_deref() == Ok("1")
    {
        use punktfunk_core::quic::HidOutput;
        // 400 ms TTL + both trigger motors. Trigger levels differ from each other and the
        // handles so a wrong-offset decoder cannot hide behind a plausible zero.
        let d = punktfunk_core::quic::encode_rumble_datagram_v3(
            0, 0x4000, 0x8000, 0, 400, 0x2000, 0x6000,
        );
        let _ = conn.send_datagram(d.to_vec());
        for h in [
            HidOutput::Led {
                pad: 0,
                r: 10,
                g: 20,
                b: 30,
            },
            HidOutput::PlayerLeds {
                pad: 0,
                bits: 0b00100,
            },
            HidOutput::Trigger {
                pad: 0,
                which: 1,
                effect: vec![0x21, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            },
        ] {
            let _ = conn.send_datagram(h.encode());
        }
        tracing::info!("PUNKTFUNK_TEST_FEEDBACK: scripted rumble + hidout burst sent");
    }

    // Native thread: no async on the hot path.
    let cfg = welcome.session_config(Role::Host);
    let source = opts.source;
    let (seconds, frames) = (opts.seconds, opts.frames);
    let mode = hello.mode;
    // `$XDG_RUNTIME_DIR/punktfunk/stream` while this session streams. RAII retracts on every exit.
    let _stream_marker = crate::stream_marker::announce(crate::stream_marker::StreamInfo {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
        hdr: welcome.color.is_hdr(),
        client: client_name.clone().unwrap_or_default(),
        fingerprint: session_fp_hex.clone(),
        launch: hello.launch.clone(),
        plane: crate::events::Plane::Native,
    });
    // Linux `PUNKTFUNK_PIN_CLOCKS`: refcounted vendor clock floor while any session streams.
    #[cfg(target_os = "linux")]
    let _clock_pin = crate::gpuclocks::session_pin();
    // One library lookup: command, title, process identity. Client picks an existing id, never
    // a command. Blocking: plugin entries ask over loopback from this async context.
    let launch_target = match hello.launch.as_deref() {
        None => None,
        Some(id) => {
            let owned = id.to_string();
            match tokio::task::spawn_blocking(move || crate::library::resolve_launch(&owned))
                .await
                .context("resolve the session's library launch")?
            {
                Some(t) => {
                    tracing::info!(
                        launch_id = id,
                        title = %t.game.title,
                        command = t.command.as_deref().unwrap_or("-"),
                        "resolved library launch for this session"
                    );
                    Some(t)
                }
                None => {
                    tracing::warn!(
                        launch_id = id,
                        "client requested a launch id not in this host's library — ignoring"
                    );
                    let _ = launch_outcome_tx.send(punktfunk_core::quic::LaunchOutcome::new(
                        punktfunk_core::quic::LaunchOutcomeKind::Refused,
                        "Couldn't start that title — this host doesn't have it in its library \
                         any more.",
                    ));
                    None
                }
            }
        }
    };
    #[cfg(target_os = "windows")]
    let launch_for_dp = launch_target.as_ref().and(hello.launch.clone());
    #[cfg(not(target_os = "windows"))]
    let launch_for_dp = launch_target.as_ref().and_then(|t| t.command.clone());
    // Reconnect inside the game's window: cancel pending termination. Data plane re-adopts via
    // `launchreg` (carries the original launch instant). Matched on (this client, this title).
    if let Some(target) = launch_target.as_ref() {
        let fp = conn.peer_fingerprint().map(hex::encode);
        // `readopt` already logged leftover processes.
        let _reprieved = crate::gamelease::readopt(fp.as_deref(), target.game.id.as_deref());
    }
    // Custom-title prep before the display opens. Drop undoes in reverse. `block_in_place`:
    // operator code is blocking and this is a multi-thread runtime.
    let _prep = hello.launch.as_deref().and_then(|id| {
        let cmds = crate::library::prep_for(id);
        // `PF_APP_ID` + `PF_STREAM_*` so a prep step can set a per-mode FPS cap.
        let mut env = vec![("PF_APP_ID".to_string(), id.to_string())];
        env.extend(crate::hooks::prep_mode_env(
            hello.mode.width,
            hello.mode.height,
            hello.mode.refresh_hz,
            welcome.color.is_hdr(),
        ));
        (!cmds.is_empty())
            .then(|| tokio::task::block_in_place(|| crate::hooks::run_prep(&cmds, &env)))
    });
    // `CLIENT_CAP_KEEP_HOST_AUDIO`: hold the wiring override before capture opens. RAII.
    let _keep_host_audio = (hello.client_caps & punktfunk_core::quic::CLIENT_CAP_KEEP_HOST_AUDIO
        != 0)
        .then(crate::audio::capture_policy::keep_host_audio_guard);
    // Welcome/acks/HUD speak wire budget. Encoder opens get the derived video rate (`EncDerive`).
    // PyroWave: budget == encoder rate (bpp pin).
    let bitrate_kbps = welcome.bitrate_kbps;
    let audio_reserved_kbps = audio_reserved_kbps(&welcome);
    // Automatic: host default. PyroWave is Automatic unconditionally (explicit rate overridden).
    let bitrate_auto = hello.bitrate_kbps == 0 || codec == crate::encode::Codec::PyroWave;
    let bit_depth = welcome.bit_depth;
    // HDR from Welcome colour, not from depth: a 10-bit SDR session is 10 + SDR.
    let hdr = welcome.color.is_hdr();
    // Typed chroma from the Welcome byte. `Yuv444` only when the handshake gate passed.
    let chroma = if welcome.chroma_format == punktfunk_core::quic::CHROMA_IDC_444 {
        crate::encode::ChromaFormat::Yuv444
    } else {
        crate::encode::ChromaFormat::Yuv420
    };
    let stop_stream = stop.clone();
    let quit_stream = quit.clone();
    let end_reason_stream = end_reason.clone();
    let counters_stream = counters.clone();
    // Client HDR volume for EDID + 0xCE. `None` = older client / no HDR → built-in defaults.
    let client_hdr = hello.display_hdr.map(crate::encode::hdr_meta_from_wire);
    let fec_target_dp = fec_target.clone();
    let conn_stream = conn.clone();
    // 0xCF host-timing only if the client advertised the cap; older clients get no extra datagrams.
    let timing_conn =
        (hello.video_caps & punktfunk_core::quic::VIDEO_CAP_HOST_TIMING != 0).then(|| conn.clone());
    // Client reassembles probe filler in its own index window. Bit clear → decline mid-session probes.
    let probe_seq = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_PROBE_SEQ != 0;
    // Sentinel-headed streamed blocks: ship early FEC while the AU tail still encodes.
    let streamed_au = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_STREAMED_AU != 0;
    // Absent ⇒ single-slice. Some TV-SoC decoders wedge on multi-slice AUs.
    let multi_slice = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE != 0;
    let stats_dp = stats;
    // Stats label: cert-fingerprint prefix, else peer IP (anonymous TOFU/--open).
    let client_label = conn
        .peer_fingerprint()
        .map(|fp| fingerprint_hex(&fp)[..12].to_string())
        .unwrap_or_else(|| conn.remote_address().ip().to_string());
    // The title's `audio.sessions`, over every session on this display. Lifted with the session.
    let _audio_policy = hello
        .launch
        .as_deref()
        .and_then(crate::library::audio_sessions_for)
        .map(|policy| crate::session_status::apply_audio_policy(policy, &client_label));
    // Punch + virtual-stream stages on the same trace; resizes write into the shared slot.
    let bringup_dp = bringup.clone();
    let resize_ms_dp = resize_ms.clone();
    // Stream thread re-points input across compositor switches and hands identity to backends.
    #[cfg(target_os = "linux")]
    let isolation_dp = isolation.clone();
    #[cfg(target_os = "linux")]
    let input_route_dp = input_route.clone();
    #[cfg(target_os = "linux")]
    let inj_shared_tx_dp = inj_tx.clone();
    #[cfg(target_os = "linux")]
    let inj_session_tx_dp = inj_session_tx.clone();
    // Control-plane local IP for the source-address check (send loop is a blocking thread).
    let control_local_ip = conn.local_ip();
    let result: Result<()> = async {
        let stream_thread = tokio::task::spawn_blocking(move || -> Result<()> {
            let (transport, wire_sock): (Box<dyn punktfunk_core::transport::Transport>, _) = match (data_plane, data_sock) {
                // A browser's video goes out on the connection it arrived on. Nothing to bind,
                // nothing to punch, and the source-address check below has no second socket to
                // compare — the datagrams leave from wherever the QUIC path already is.
                (DataPlane::Web(plane), _) => {
                    bringup_dp.mark("punch_done");
                    (Box::new(plane), None)
                }
                (DataPlane::Udp, None) => anyhow::bail!("the native plane negotiated no data socket"),
                (DataPlane::Udp, Some(data_sock)) => {
            // Wait for the client's punch and stream to its observed source — the port a NAT or
            // a port proxy actually answers from. A fixed --data-port is no exception (it fixes
            // only the host's side), so the client-reported port is the fallback for a punch
            // that never arrives, never the first choice. IP is the host-observed QUIC remote.
            let bound = UdpTransport::from_socket_punch(
                data_sock,
                &client_udp.to_string(),
                client_udp.ip(),
                std::time::Duration::from_millis(2500),
            );
            let (transport, punched) = match bound {
                Ok(v) => v,
                Err(e) => {
                    // Surface here: a teardown stall would otherwise swallow a bind error.
                    tracing::error!(error = %e, %client_udp, udp_port, "data-plane socket setup failed");
                    return Err(anyhow::Error::new(e)).context("bind data plane");
                }
            };
            bringup_dp.mark("punch_done");
            // Post-`connect` `local_addr` is the source stamped on every video datagram.
            let local = transport.local_addr().ok();
            tracing::info!(
                %client_udp,
                udp_port,
                punched,
                local = ?local,
                "data plane bound (punched=true → the client's observed source; false → no \
                 punch seen, the reported address)"
            );
            // Wrong egress: the client's connected socket drops every datagram before userspace.
            if let (Some(l), Some(c)) = (local.map(|a| a.ip()), control_local_ip) {
                let c = match c {
                    std::net::IpAddr::V6(v6) => {
                        v6.to_ipv4_mapped().map_or(c, std::net::IpAddr::V4)
                    }
                    v4 => v4,
                };
                if !l.is_unspecified() && l != c {
                    tracing::warn!(
                        video_source_ip = %l,
                        control_local_ip = %c,
                        "the video data plane egresses from a DIFFERENT host address than the one \
                         this client connected to — its data socket is connected to the address it \
                         dialed, so its kernel drops every video datagram before userspace: black \
                         screen, zero reported loss, healthy control plane. Usual cause is two \
                         live paths to the client (Ethernet and Wi-Fi both up on the same LAN, or \
                         a VPN/overlay adapter claiming the route)"
                    );
                }
            }
            // No punch: inbound UDP to this port looks blocked, fixed or not, and video now goes
            // to an address the client only claimed.
            if !punched {
                tracing::warn!(
                    %client_udp,
                    udp_port,
                    "no hole-punch reached this host's data port — inbound UDP to it looks \
                     BLOCKED, so video is being sent to the address the client reported without \
                     any confirmed return path. If the picture stays black while the session is \
                     otherwise healthy, this line is the reason: allow inbound UDP for the host \
                     executable (any port), or pin --data-port and open that one"
                );
            }
            let wire_sock = transport.try_clone_socket().ok();
            (Box::new(transport), wire_sock)
                }
            };
            let mut session = Session::new(cfg, transport)
                .map_err(|e| anyhow!("host session: {e:?}"))?;
            match source {
                Punktfunk1Source::Software => software_stream(
                    &mut session,
                    codec,
                    mode,
                    bitrate_kbps,
                    &stop_stream,
                    &probe_rx,
                    &probe_result_tx,
                    &fec_target_dp,
                    probe_seq,
                ),
                Punktfunk1Source::Synthetic => synthetic_stream(
                    &mut session,
                    frames,
                    &stop_stream,
                    &probe_rx,
                    &probe_result_tx,
                    &fec_target_dp,
                    timing_conn.as_ref(),
                    probe_seq,
                ),
                Punktfunk1Source::Virtual => {
                    let compositor = compositor
                        .expect("the Virtual source resolves a compositor during the handshake");
                    let ctx = SessionContext {
                        session,
                        mode,
                        seconds,
                        stop: stop_stream,
                        quit: quit_stream,
                        end_reason: end_reason_stream,
                        counters: counters_stream,
                        reconfig: reconfig_rx,
                        keyframe: keyframe_rx,
                        rfi: rfi_rx,
                        bitrate_rx,
                        shard_rx: shard_apply_rx,
                        compositor,
                        gamescope_route,
                        bitrate_kbps,
                        audio_reserved_kbps,
                        shard_payload: welcome.shard_payload,
                        live_bitrate,
                        encoder_ceiling,
                        cadence_degraded,
                        cadence_behind_score,
                        client_packets_received,
                        bitrate_auto,
                        bit_depth,
                        hdr,
                        chroma,
                        codec,
                        probe_rx,
                        probe_result_tx,
                        reconfig_result_tx,
                        retarget_tx,
                        gap_tx,
                        fec_target: fec_target_dp,
                        phase: phase_ctl,
                        conn: conn_stream,
                        timing_conn,
                        cursor_forward,
                        cursor_shape_tx,
                        cursor_client_draws: cursor_client_draws_dp,
                        probe_seq,
                        streamed_au,
                        multi_slice,
                        stats: stats_dp,
                        client_label,
                        client_name,
                        launch: launch_for_dp,
                        launch_target,
                        launch_outcome: launch_outcome_dp,
                        client_hdr,
                        join_live,
                        controls,
                        reframe_to,
                        frame_map,
                        bringup: bringup_dp,
                        resize_ms: resize_ms_dp,
                        wire_sock,
                        #[cfg(target_os = "linux")]
                        input_tx: input_tx_stream,
                        #[cfg(target_os = "linux")]
                        isolation: isolation_dp,
                        #[cfg(target_os = "linux")]
                        input_route: input_route_dp,
                        #[cfg(target_os = "linux")]
                        inj_shared_tx: inj_shared_tx_dp,
                        #[cfg(target_os = "linux")]
                        inj_session_tx: inj_session_tx_dp,
                    };
                    match prep {
                        // Display prep started at Welcome: hand it the post-punch context.
                        Some((ctx_tx, prep_thread)) => match ctx_tx.send(ctx) {
                            Ok(()) => match prep_thread.join() {
                                Ok(r) => r,
                                Err(_) => Err(anyhow!("prepared stream thread panicked")),
                            },
                            // Prep died before hand-off (guard/lease unwound): build inline.
                            Err(std::sync::mpsc::SendError(ctx)) => {
                                tracing::warn!(
                                    "display-prep thread gone before hand-off — building inline"
                                );
                                virtual_stream(ctx, None)
                            }
                        },
                        None => virtual_stream(ctx, None),
                    }
                }
            }
        });
        // `stop` is advisory: a stuck syscall inside an iteration never sees it, and teardown
        // waits on this join. Bound the wait: after `STREAM_STOP_GRACE`, abandon the thread
        // (cannot cancel a blocking thread) so the slot and admission entry come back.
        tokio::select! {
            joined = stream_thread => joined.context("stream thread")??,
            () = stop_overdue(&stop) => {
                tracing::error!(
                    grace_s = STREAM_STOP_GRACE.as_secs(),
                    "stream thread has not returned since the session was stopped — abandoning it so \
                     the session slot is freed. Its capture/encoder stay held until the stuck call \
                     returns; this is a HOST WEDGE — please report it with the log above"
                );
                anyhow::bail!("stream thread wedged after stop");
            }
        }
        // Drain window before close.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        Ok(())
    }
    .await;

    // Every path: stop audio, close, join side threads. Close ends the datagram task → input.
    stop.store(true, Ordering::SeqCst);
    conn.close(
        if result.is_ok() { 0u32 } else { 1u32 },
        if result.is_ok() { b"done" } else { b"error" },
    );
    // Bounded join: a stuck side thread must not hold the permit/admission entry.
    let side_threads = tokio::task::spawn_blocking(move || {
        if let Some(h) = audio_handle {
            let _ = h.join();
        }
        let _ = input_handle.join();
    });
    if tokio::time::timeout(SIDE_THREAD_JOIN_GRACE, side_threads)
        .await
        .is_err()
    {
        // Input thread still owns the virtual pads (Windows: devnode + pad-index mailbox).
        // The next create on that index fails as already-owned until this thread returns.
        tracing::warn!(
            grace_s = SIDE_THREAD_JOIN_GRACE.as_secs(),
            "audio/input threads did not exit after the connection closed — detaching them. This \
             session's virtual gamepads are STILL HELD by the detached input thread (devnode + \
             pad-index mailbox on Windows), so a pad create on the same index will be refused as \
             already-owned until it returns"
        );
    }
    // Managed gamescope on an autologin box: put the TV's gaming session back once no session
    // streams gamescope. A `join` session still shows the owner's game after the owner leaves.
    drop(gamescope_hold);
    if LIVE_GAMESCOPE.load(Ordering::SeqCst) == 0 {
        crate::vdisplay::restore_managed_session();
    }
    result.map(|()| Served::Session)
}

/// Live native sessions on a gamescope display, for the managed TV restore above.
static LIVE_GAMESCOPE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// One count in [`LIVE_GAMESCOPE`] for the session's lifetime, early returns included.
struct GamescopeHold;

impl GamescopeHold {
    fn new() -> Self {
        LIVE_GAMESCOPE.fetch_add(1, Ordering::SeqCst);
        GamescopeHold
    }
}

impl Drop for GamescopeHold {
    fn drop(&mut self) {
        LIVE_GAMESCOPE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Reopen backoff after a host-lifetime capturer dies. Mic has its own ([`crate::audio::MicPump`]).
const INJECTOR_REOPEN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// Pack `(w, h, hz)` into one atomic word (16|16|16) — one store, not three racy ones.
pub(crate) fn pack_mode(width: u32, height: u32, refresh_hz: u32) -> u64 {
    ((width as u64 & 0xffff) << 32)
        | ((height as u64 & 0xffff) << 16)
        | (refresh_hz as u64 & 0xffff)
}

pub(crate) fn unpack_mode(packed: u64) -> (u32, u32, u32) {
    (
        ((packed >> 32) & 0xffff) as u32,
        ((packed >> 16) & 0xffff) as u32,
        (packed & 0xffff) as u32,
    )
}

/// Integer Hz from `1/effective_hz` (exact). Differs from the request when e.g. KWin caps at 60.
fn interval_hz(interval: std::time::Duration) -> u32 {
    (1.0 / interval.as_secs_f64()).round() as u32
}

/// Mode the pipeline is actually delivering, for a corrective `Reconfigured` ack. Diverges
/// when a backend cannot honor the request (KWin refresh cap; Windows `SetMode` not in EDID).
fn delivered_mode(
    frame_width: u32,
    frame_height: u32,
    interval: std::time::Duration,
) -> punktfunk_core::Mode {
    punktfunk_core::Mode {
        width: frame_width,
        height: frame_height,
        refresh_hz: interval_hz(interval),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The accept loop's address-validation gate. A first contact is unvalidated; a Retry turns
    /// it into a second, validated arrival, and the client completes anyway. Pins the quinn
    /// behaviour the gate rests on — a release that validated first contact would leave the
    /// branch dead, and one that refused a legal retry would drop every new client.
    #[test]
    fn an_unvalidated_first_contact_is_retried_then_accepted() {
        use punktfunk_core::quic::endpoint;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = server.local_addr().unwrap();
            let accept = tokio::spawn(async move {
                let mut arrivals = Vec::new();
                while let Some(incoming) = server.accept().await {
                    let validated = incoming.remote_address_validated();
                    arrivals.push(validated);
                    // The same gate `serve` applies before it spends a task on a source.
                    if !validated {
                        assert!(
                            incoming.retry().is_ok(),
                            "retry is legal whenever the source is unvalidated"
                        );
                        continue;
                    }
                    let conn = incoming.await.expect("host side of the retried handshake");
                    // Hold the endpoint: dropping it would close the connection under the client.
                    return (arrivals, server, conn);
                }
                panic!("endpoint closed before a validated arrival");
            });
            let client = endpoint::client_insecure().unwrap();
            let client_conn = client
                .connect(addr, "punktfunk")
                .unwrap()
                .await
                .expect("client completes across the Retry");
            let (arrivals, _server, _host_conn) = accept.await.unwrap();
            // An Initial the client retransmits before the Retry lands arrives unvalidated too,
            // so pin the shape rather than the count: every arrival we turned away was
            // unvalidated, and the one we accepted had proved its address.
            let (accepted, retried) = arrivals.split_last().expect("at least one arrival");
            assert!(
                accepted,
                "the accepted arrival is address-validated: {arrivals:?}"
            );
            assert!(
                !retried.is_empty() && retried.iter().all(|v| !v),
                "first contact is unvalidated and gets retried: {arrivals:?}"
            );
            drop(client_conn);
        });
    }

    /// The pipeline raises a pin miss under two layers of `.context`, so the close
    /// carries the user's sentence only if the downcast walks the whole chain.
    /// Reads the error `resolve` really produces, not a stand-in.
    #[test]
    fn a_buried_pin_miss_still_reaches_the_client_in_words() {
        use anyhow::Context;
        let heads = [pf_vdisplay::monitors::PhysicalMonitor {
            connector: "HDMI-A-1".into(),
            description: String::new(),
            width: 1920,
            height: 1080,
            refresh_mhz: 60000,
            x: 0,
            y: 0,
            scale: 1.0,
            primary: true,
            enabled: true,
            managed: false,
        }];
        let buried = pf_vdisplay::monitors::resolve(&heads, "HDMI-A-3")
            .map(|_| ())
            .context("create virtual output")
            .context("build the session pipeline")
            .unwrap_err();

        let said = setup_failed_sentence(&buried).expect("a pin miss has words for the user");
        assert!(
            said.contains("HDMI-A-3") && said.contains("HDMI-A-1"),
            "{said}"
        );
        assert!(
            said.len() <= punktfunk_core::quic::REFUSED_REASON_MAX,
            "one close frame, uncut: {} bytes",
            said.len()
        );

        // Anything else keeps the client's own wording rather than leaking a chain.
        let other =
            anyhow::anyhow!("open NVENC: device busy").context("build the session pipeline");
        assert_eq!(setup_failed_sentence(&other), None);
    }

    #[test]
    fn full_apply_readback_is_the_request_not_the_deflated_roundtrip() {
        // A roundtrip that lost only truncation must report the full request, not a phantom ceiling.
        let ed = EncDerive {
            audio_kbps: 576,
            shard_payload: 1408,
            fec_percent: 8,
            identity: false,
        };
        for budget in [2349u32, 4799, 6857, 9798, 14000, 20000, 940_032] {
            let asked_enc = ed.enc_kbps(budget);
            assert!(
                ed.budget_kbps(asked_enc) <= budget,
                "roundtrip must not inflate"
            );
            assert_eq!(ed.applied_budget_kbps(budget, asked_enc), budget);
        }
        // A genuine driver short-apply still reports short.
        let asked_enc = ed.enc_kbps(1_010_000);
        let short = ed.applied_budget_kbps(1_010_000, asked_enc * 3 / 4);
        assert!(short < ed.budget_kbps(ed.enc_kbps(1_010_000)));
    }

    /// One short apply is a ceiling, not a life sentence: it holds for its wait,
    /// then the next ask reaches the encoder. A full apply there drops it; a
    /// short one puts it back with twice the wait.
    #[test]
    fn a_learned_encoder_ceiling_is_re_tested_and_backs_off() {
        let mut c = EncoderCeiling::new();
        assert_eq!(c.resolve(400_000), (400_000, AckReason::Granted));
        c.note_applied(400_000, 300_000);
        let first_wait = c.wait();
        // Under the ceiling nothing is refused; above it, the ack says why.
        assert_eq!(c.resolve(200_000), (200_000, AckReason::Granted));
        assert_eq!(c.resolve(400_000), (300_000, AckReason::EncoderLimit));
        // Past the wait, one ask reaches the encoder.
        c.spend_the_wait();
        assert_eq!(c.resolve(400_000), (400_000, AckReason::Granted));
        // Still there: re-learned, and the next wait is twice as long.
        c.note_applied(400_000, 300_000);
        assert_eq!(c.wait(), first_wait * 2);
        assert_eq!(c.resolve(400_000), (300_000, AckReason::EncoderLimit));
        // Gone: the ceiling goes with it, and nothing is clamped again.
        c.spend_the_wait();
        assert_eq!(c.resolve(400_000), (400_000, AckReason::Granted));
        c.note_applied(400_000, 400_000);
        assert_eq!(c.resolve(8_000_000), (8_000_000, AckReason::Granted));
    }

    /// The encoder that refused a rate is gone (a mode switch, a rebuild on a
    /// new source), and so is what it taught.
    #[test]
    fn a_rebuilt_encoder_starts_with_no_ceiling() {
        let mut c = EncoderCeiling::new();
        c.note_applied(400_000, 300_000);
        assert_eq!(c.resolve(400_000), (300_000, AckReason::EncoderLimit));
        c.clear();
        assert_eq!(c.resolve(400_000), (400_000, AckReason::Granted));
        // And the clock starts over rather than carrying the old backoff.
        c.note_applied(400_000, 300_000);
        assert_eq!(c.wait(), punktfunk_core::abr::WINDOW * 16);
    }

    #[test]
    fn live_mode_pack_roundtrips_and_interval_recovers_hz() {
        // Pack → unpack is exact for real modes.
        for (w, h, hz) in [(1280u32, 720u32, 60u32), (3840, 2160, 144), (320, 200, 24)] {
            assert_eq!(unpack_mode(pack_mode(w, h, hz)), (w, h, hz));
        }
        // `interval` is 1/effective_hz — the round-trip recovers the integer rate.
        for hz in [24u32, 30, 60, 75, 90, 120, 144, 165, 240] {
            let interval = std::time::Duration::from_secs_f64(1.0 / hz as f64);
            assert_eq!(interval_hz(interval), hz);
        }
    }

    #[test]
    fn delivered_mode_reports_captured_dims_and_triggers_corrective_ack() {
        let hz60 = std::time::Duration::from_secs_f64(1.0 / 60.0);
        let requested = punktfunk_core::Mode {
            width: 2560,
            height: 1440,
            refresh_hz: 60,
        };

        // Honored: captured frame matches the request → no corrective ack.
        let honored = delivered_mode(2560, 1440, hz60);
        assert_eq!(honored, requested);

        // Fallback dims differ from the acked request → a corrective ack is owed.
        let fell_back = delivered_mode(1920, 1080, hz60);
        assert_ne!(fell_back, requested);
        assert_eq!(
            fell_back,
            punktfunk_core::Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60
            }
        );

        // Refresh cap: same dims, achieved rate recovered from the interval.
        let capped = delivered_mode(2560, 1440, std::time::Duration::from_secs_f64(1.0 / 30.0));
        assert_ne!(capped, requested);
        assert_eq!(capped.refresh_hz, 30);
    }

    #[test]
    fn pyrowave_bitrate_pins_to_bpp_default() {
        use punktfunk_core::config::Mode;
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        use crate::encode::ChromaFormat;
        // Automatic PyroWave → ~1.6 bpp, not the 20 Mbps H.26x default.
        let kbps = resolve_bitrate_kbps_for(
            crate::encode::Codec::PyroWave,
            0,
            &mode,
            ChromaFormat::Yuv420,
            8,
        );
        assert_eq!(kbps, 1920 * 1080 * 60 * 16 / 10 / 1000);
        // 4:4:4 ≈ 2.6 bpp; 10-bit adds 15 %. `design/pyrowave-444-hdr.md`.
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::PyroWave,
                0,
                &mode,
                ChromaFormat::Yuv444,
                8
            ),
            1920 * 1080 * 60 * 26 / 10 / 1000
        );
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::PyroWave,
                0,
                &mode,
                ChromaFormat::Yuv444,
                10
            ),
            (1920u64 * 1080 * 60 * 26 / 10 * 115 / 100 / 1000) as u32
        );
        // Explicit client rate is overridden to the same pin (kbps is ill-defined for all-intra).
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::PyroWave,
                130_000,
                &mode,
                ChromaFormat::Yuv420,
                8
            ),
            1920 * 1080 * 60 * 16 / 10 / 1000
        );
        // H.26x codecs keep the 20 Mbps default.
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::H265,
                0,
                &mode,
                ChromaFormat::Yuv420,
                8
            ),
            DEFAULT_BITRATE_KBPS
        );
    }

    #[test]
    fn pyrowave_auto_pin_respects_operator_ceiling() {
        use crate::encode::{ChromaFormat, Codec};
        use punktfunk_core::config::Mode;
        // 5120×1440@240 4:4:4 10-bit pins above a 5 GbE link.
        let mode = Mode {
            width: 5120,
            height: 1440,
            refresh_hz: 240,
        };
        let uncapped =
            resolve_bitrate_kbps_for(Codec::PyroWave, 0, &mode, ChromaFormat::Yuv444, 10);
        assert!(
            uncapped > 5_000_000,
            "expected the open-loop pin, got {uncapped}"
        );
        // Ceiling caps the Automatic pin to the link rate.
        // SAFETY: this test is the only writer of this variable in the process; the only
        // reader is `resolve_bitrate_kbps_for` on this same thread.
        unsafe { std::env::set_var("PUNKTFUNK_PYROWAVE_MAX_MBPS", "4500") };
        pf_host_config::reload();
        assert_eq!(
            resolve_bitrate_kbps_for(Codec::PyroWave, 0, &mode, ChromaFormat::Yuv444, 10),
            4_500_000
        );
        // A pin already under the ceiling is untouched.
        let small = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        assert_eq!(
            resolve_bitrate_kbps_for(Codec::PyroWave, 0, &small, ChromaFormat::Yuv420, 8),
            1920 * 1080 * 60 * 16 / 10 / 1000
        );
        // Explicit client rate still goes through pin + ceiling.
        assert_eq!(
            resolve_bitrate_kbps_for(Codec::PyroWave, 6_000_000, &mode, ChromaFormat::Yuv444, 10),
            4_500_000
        );
        // SAFETY: same as the set above — single writer; readers run on this thread.
        unsafe { std::env::remove_var("PUNKTFUNK_PYROWAVE_MAX_MBPS") };
        pf_host_config::reload();
    }

    /// An RFI ask prices the report window it lands in. A report a window late closes a
    /// window the client discarded on purpose (probe tail, pipeline gap), so the asks before
    /// it must not leak into the clean window that follows.
    #[test]
    fn an_rfi_from_a_discarded_window_does_not_price_the_next_report() {
        let w = punktfunk_core::client::ADAPT_REPORT_INTERVAL;
        let t0 = std::time::Instant::now();
        let mut run = UnrecoveredRun::default();
        assert_eq!(run.report(t0), 0);
        run.rfi();
        assert_eq!(run.report(t0 + w), 1);
        run.rfi();
        run.rfi();
        assert_eq!(run.report(t0 + w * 2), 2, "several asks are one window");
        assert_eq!(run.report(t0 + w * 3), 0, "a clean window ends the run");
        // An ask, then a discarded window: the report lands two windows after the last.
        run.rfi();
        assert_eq!(run.report(t0 + w * 5), 0);
        run.rfi();
        assert_eq!(
            run.report(t0 + w * 6),
            1,
            "the next on-time window counts again"
        );
    }

    #[test]
    fn data_socket_defaults_to_an_ephemeral_port() {
        // No fixed port (and 0) → some ephemeral port, never 0.
        for req in [None, Some(0)] {
            let sock = bind_data_socket(req, None).expect("bind random data socket");
            assert_ne!(sock.local_addr().unwrap().port(), 0);
        }
    }

    #[test]
    fn data_socket_fixed_binds_it_then_falls_back_when_busy() {
        // Reserve-then-rebind like the host. A race here is flaky, not wrong.
        let free = std::net::UdpSocket::bind("0.0.0.0:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        // Free fixed port binds exactly it.
        let held = bind_data_socket(Some(free), None).expect("bind fixed data socket");
        assert_eq!(held.local_addr().unwrap().port(), free);

        // Busy fixed port falls back to ephemeral, not fail.
        let fallback = bind_data_socket(Some(free), None).expect("busy fixed port falls back");
        assert_ne!(
            fallback.local_addr().unwrap().port(),
            free,
            "the fallback must not reuse the busy fixed port"
        );
    }

    /// Video must egress from the address the control connection arrived on. The client's
    /// data socket is connected to the host IP it dialed; any other source is dropped before
    /// userspace. A wildcard bind lets the routing table pick a different interface.
    #[test]
    fn data_socket_binds_the_address_the_control_plane_arrived_on() {
        let loopback = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        let sock = bind_data_socket(None, Some(loopback)).expect("bind pinned data socket");
        assert_eq!(sock.local_addr().unwrap().ip(), loopback);

        // Dual-stack reports IPv4-mapped v6; unmap or the socket binds v6 and cannot `connect` v4.
        let mapped = std::net::IpAddr::V6(std::net::Ipv4Addr::LOCALHOST.to_ipv6_mapped());
        let sock = bind_data_socket(None, Some(mapped)).expect("bind mapped data socket");
        assert_eq!(sock.local_addr().unwrap().ip(), loopback);

        // No reported local address keeps the wildcard.
        let sock = bind_data_socket(None, None).expect("bind wildcard data socket");
        assert!(sock.local_addr().unwrap().ip().is_unspecified());
    }

    /// A pinned address that this host cannot own still has to yield a socket. 192.0.2.1
    /// is TEST-NET-1, so the bind miss is the live fallback, not a mock.
    #[test]
    fn data_socket_falls_back_to_wildcard_when_the_control_address_cannot_bind() {
        let unroutable = std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 1));
        let sock = bind_data_socket(None, Some(unroutable))
            .expect("wildcard fallback after a pinned bind miss");
        assert!(sock.local_addr().unwrap().ip().is_unspecified());
    }

    /// Pin every button bit and axis id. Native and GameStream share `core::input::gamepad`;
    /// renumbering a bit silently breaks shipped clients.
    #[test]
    fn gamepad_wire_bits_are_pinned() {
        use punktfunk_core::input::gamepad as pf;
        // buttonFlags — low 16 bits, named from core.
        assert_eq!(pf::BTN_DPAD_UP, 0x0000_0001);
        assert_eq!(pf::BTN_DPAD_DOWN, 0x0000_0002);
        assert_eq!(pf::BTN_DPAD_LEFT, 0x0000_0004);
        assert_eq!(pf::BTN_DPAD_RIGHT, 0x0000_0008);
        assert_eq!(pf::BTN_START, 0x0000_0010);
        assert_eq!(pf::BTN_BACK, 0x0000_0020);
        assert_eq!(pf::BTN_LS_CLICK, 0x0000_0040);
        assert_eq!(pf::BTN_RS_CLICK, 0x0000_0080);
        assert_eq!(pf::BTN_LB, 0x0000_0100);
        assert_eq!(pf::BTN_RB, 0x0000_0200);
        assert_eq!(pf::BTN_GUIDE, 0x0000_0400);
        assert_eq!(pf::BTN_A, 0x0000_1000);
        assert_eq!(pf::BTN_B, 0x0000_2000);
        assert_eq!(pf::BTN_X, 0x0000_4000);
        assert_eq!(pf::BTN_Y, 0x0000_8000);
        // buttonFlags2 — paddles + DualSense/DS4 touchpad-click / Share.
        assert_eq!(pf::BTN_PADDLE1, 0x0001_0000);
        assert_eq!(pf::BTN_PADDLE2, 0x0002_0000);
        assert_eq!(pf::BTN_PADDLE3, 0x0004_0000);
        assert_eq!(pf::BTN_PADDLE4, 0x0008_0000);
        assert_eq!(pf::BTN_TOUCHPAD, 0x0010_0000);
        assert_eq!(pf::BTN_MISC1, 0x0020_0000);
        // Axis ids — dense, 0-based.
        assert_eq!(
            [
                pf::AXIS_LS_X,
                pf::AXIS_LS_Y,
                pf::AXIS_RS_X,
                pf::AXIS_RS_Y,
                pf::AXIS_LT,
                pf::AXIS_RT,
            ],
            [0, 1, 2, 3, 4, 5]
        );
    }

    /// Pull and byte-verify `count` synthetic frames through the C ABI connection.
    unsafe fn pull_verified(conn: *mut punktfunk_core::abi::PunktfunkConnection, count: u32) {
        use punktfunk_core::error::PunktfunkStatus;
        let mut got = 0u32;
        // SAFETY: `PunktfunkFrame` is `#[repr(C)]` POD; all-zero is valid (null `data`, `len == 0`).
        // Read only after `next_au` overwrites it on `Ok`.
        let mut frame = unsafe { std::mem::zeroed() };
        while got < count {
            // SAFETY: `conn` is the live handle from `punktfunk_connect` (caller asserts non-null,
            // does not close until after return). `&mut frame` outlives this call. This thread is
            // the only video puller.
            match unsafe {
                punktfunk_core::abi::punktfunk_connection_next_au(conn, &mut frame, 2000)
            } {
                PunktfunkStatus::Ok => {
                    // SAFETY: on `Ok`, `frame.data`/`len` is the connection-owned AU, valid until the
                    // next `next_au` on this handle. We read the whole slice before that next call.
                    let data = unsafe { std::slice::from_raw_parts(frame.data, frame.len) };
                    let idx = u32::from_le_bytes(data[0..4].try_into().unwrap());
                    assert_eq!(
                        data,
                        &test_frame(idx, data.len())[..],
                        "frame {idx} content"
                    );
                    got += 1;
                }
                PunktfunkStatus::NoFrame => continue,
                other => panic!("next_au: {other:?} after {got} of {count} frames"),
            }
        }
    }

    /// In-process hosts share the process-global admission table. Concurrent tests would
    /// `preempt_same_identity` each other. Poison-tolerant so a failing test does not cascade.
    static SESSION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// C ABI: TOFU connect → pull frames → send input → close. Three sequential sessions
    /// against one host prove the persistent listener; a wrong pin is rejected.
    #[test]
    fn c_abi_connection_roundtrip() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::abi::{
            punktfunk_connect, punktfunk_connection_close, punktfunk_connection_mode,
            punktfunk_connection_send_input,
        };
        use punktfunk_core::error::PunktfunkStatus;

        let host = std::thread::spawn(|| {
            run_ephemeral(Punktfunk1Options {
                port: 19777,
                source: Punktfunk1Source::Synthetic,
                seconds: 0,
                // More than the 25 each session pulls. The budget is one loop per session and a
                // mid-stream mode change discards what is already queued at the old mode, so a
                // client that pulls the whole budget only succeeds when its switch beats frame 0.
                frames: 40,
                max_sessions: 3,
                max_concurrent: 1,
                require_pairing: false,
                allow_pairing: false,
                pairing_pin: None,
                paired_store: None,
                data_port: None,
                idle_timeout: None,
                mdns: false, // tests must not advertise on the LAN
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Session 1: TOFU (no pin) — observe the host fingerprint.
        let addr = std::ffi::CString::new("127.0.0.1").unwrap();
        let mut observed = [0u8; 32];
        // SAFETY: `addr` is a live NUL-terminated host string; pin/cert/key are NULL (permitted);
        // `observed` is 32 writable bytes. All locals outlive the blocking connect.
        let conn = unsafe {
            punktfunk_connect(
                addr.as_ptr(),
                19777,
                1280,
                720,
                60,
                std::ptr::null(),
                observed.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                10_000,
            )
        };
        assert!(!conn.is_null(), "punktfunk_connect failed");
        assert_ne!(observed, [0u8; 32], "fingerprint not reported");

        let (mut w, mut h, mut hz) = (0u32, 0u32, 0u32);
        // SAFETY: `conn` is the live handle; `&mut w/h/hz` outlive this call.
        let st = unsafe { punktfunk_connection_mode(conn, &mut w, &mut h, &mut hz) };
        assert_eq!(st, PunktfunkStatus::Ok);
        assert_eq!((w, h, hz), (1280, 720, 60));

        // Mid-stream renegotiation: request a new mode; `punktfunk_connection_mode` reflects it.
        // SAFETY: `conn` is the live handle; remaining args are by-value. Handle outlives enqueue.
        let st = unsafe {
            punktfunk_core::abi::punktfunk_connection_request_mode(conn, 1920, 1080, 144)
        };
        assert_eq!(st, PunktfunkStatus::Ok);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // SAFETY: same as the earlier `punktfunk_connection_mode` call.
            let st = unsafe { punktfunk_connection_mode(conn, &mut w, &mut h, &mut hz) };
            assert_eq!(st, PunktfunkStatus::Ok);
            if (w, h, hz) == (1920, 1080, 144) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "mode switch not acked (still {w}x{h}@{hz})"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // SAFETY: `conn` is the open handle; this thread is the only video puller.
        unsafe { pull_verified(conn, 25) };

        let ev = punktfunk_core::input::InputEvent {
            kind: punktfunk_core::input::InputKind::MouseMove,
            _pad: [0; 3],
            code: 0,
            x: 1,
            y: 2,
            flags: 0,
        };
        // SAFETY: `conn` is live; `&ev` is a valid `InputEvent` for this enqueue.
        let st = unsafe { punktfunk_connection_send_input(conn, &ev) };
        assert_eq!(st, PunktfunkStatus::Ok);
        // SAFETY: `conn` is unused after this; `close` frees it once. Session 2 uses `conn2`.
        unsafe { punktfunk_connection_close(conn) };

        // Session 2 (same host process): pin the fingerprint.
        // SAFETY: as session 1 — `observed.as_ptr()` is the 32-byte pin; out/cert/key are NULL.
        let conn2 = unsafe {
            punktfunk_connect(
                addr.as_ptr(),
                19777,
                1280,
                720,
                60,
                observed.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                10_000,
            )
        };
        assert!(!conn2.is_null(), "pinned reconnect failed");
        // SAFETY: `conn2` is the live pinned handle; this thread is the only puller.
        unsafe { pull_verified(conn2, 25) };
        // SAFETY: `conn2` is unused after this; `close` frees it once.
        unsafe { punktfunk_connection_close(conn2) };

        // Session 3: a wrong pin must be rejected.
        let bad = [0xAAu8; 32];
        // SAFETY: `bad.as_ptr()` is the 32-byte pin; out/cert/key are NULL. Expected to return NULL.
        let conn3 = unsafe {
            punktfunk_connect(
                addr.as_ptr(),
                19777,
                1280,
                720,
                60,
                bad.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                10_000,
            )
        };
        assert!(conn3.is_null(), "wrong pin must fail the handshake");

        // TLS-failed handshake never yields a connection, so accept() is still waiting.
        // One more TOFU connect completes the host's third session.
        // SAFETY: same as session 1 — pin/out/cert/key all NULL.
        let conn4 = unsafe {
            punktfunk_connect(
                addr.as_ptr(),
                19777,
                1280,
                720,
                60,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                10_000,
            )
        };
        assert!(!conn4.is_null());
        // SAFETY: `conn4` is live; this thread is the only puller.
        unsafe { pull_verified(conn4, 25) };
        // SAFETY: `conn4` is unused after this; `close` frees it once.
        unsafe { punktfunk_connection_close(conn4) };

        host.join().unwrap().unwrap();
    }

    /// Clipboard over a synthetic session: host advertises the cap, acks enable with
    /// `BACKEND_UNAVAILABLE` (no compositor), declines a fetch. Live-backend paths are
    /// not covered here. `design/clipboard-and-file-transfer.md`.
    #[test]
    fn clipboard_control_and_fetch_decline_over_session() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::NativeClient;
        use punktfunk_core::clipboard::ClipEventCore;
        use punktfunk_core::quic::{
            CLIP_FILE_INDEX_NONE, CLIP_FLAG_FILES, CLIP_POLICY_FILES, HOST_CAP_CLIPBOARD,
        };

        // Restore the env even on panic so a leaked var cannot reach the next session test.
        struct EnvGuard(&'static str);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: dropped while SESSION_TEST_LOCK is held; only the session path reads this.
                unsafe { std::env::remove_var(self.0) };
                pf_host_config::reload();
            }
        }
        let _env = EnvGuard("PUNKTFUNK_CLIPBOARD");
        // Operator policy on. Serialized on SESSION_TEST_LOCK; only the session path reads this.
        // SAFETY: writers serialized; only this session path reads the variable.
        unsafe { std::env::set_var("PUNKTFUNK_CLIPBOARD", "1") };
        pf_host_config::reload();

        let host = std::thread::spawn(|| {
            run_ephemeral(Punktfunk1Options {
                port: 19781,
                source: Punktfunk1Source::Synthetic,
                seconds: 0,
                frames: 600, // outlive the control exchange
                max_sessions: 1,
                max_concurrent: 1,
                require_pairing: false,
                allow_pairing: false,
                pairing_pin: None,
                paired_store: None,
                data_port: None,
                idle_timeout: None,
                mdns: false,
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(500));

        let mode = punktfunk_core::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let client = NativeClient::connect(
            "127.0.0.1",
            19781,
            mode,
            CompositorPref::Auto,
            GamepadPref::Auto,
            0,
            0,
            2,
            0,
            0,
            None,
            0,
            false,
            None,
            None,
            None,
            None,
            std::time::Duration::from_secs(10),
        )
        .expect("client connects to synthetic host");

        assert_ne!(
            client.host_caps() & HOST_CAP_CLIPBOARD,
            0,
            "an enabled host advertises HOST_CAP_CLIPBOARD"
        );

        // Bounded poll over the clipboard event plane.
        let poll = |pred: &dyn Fn(&ClipEventCore) -> bool| -> Option<ClipEventCore> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                match client.next_clip(std::time::Duration::from_millis(200)) {
                    Ok(ev) if pred(&ev) => return Some(ev),
                    Ok(_) => {}
                    Err(punktfunk_core::PunktfunkError::NoFrame) => {}
                    Err(_) => break,
                }
            }
            None
        };

        // Enable (files): synthetic has no backend → BACKEND_UNAVAILABLE, policy still reports files.
        client.clip_control(true, CLIP_FLAG_FILES).unwrap();
        let state = poll(&|e| matches!(e, ClipEventCore::State { .. }))
            .expect("host replies with a ClipState ack");
        match state {
            ClipEventCore::State {
                enabled,
                policy,
                reason,
            } => {
                assert!(!enabled, "no backend for a synthetic session → not enabled");
                assert_eq!(
                    reason,
                    punktfunk_core::quic::CLIP_REASON_BACKEND_UNAVAILABLE,
                    "the refusal reason is BACKEND_UNAVAILABLE"
                );
                assert_ne!(
                    policy & CLIP_POLICY_FILES,
                    0,
                    "PUNKTFUNK_CLIPBOARD=1 permits files"
                );
            }
            _ => unreachable!(),
        }

        // Fetch: no backend → Error for that transfer id.
        let xfer = client
            .clip_fetch(1, "text/plain;charset=utf-8".into(), CLIP_FILE_INDEX_NONE)
            .unwrap();
        let err = poll(&|e| matches!(e, ClipEventCore::Error { id, .. } if *id == xfer))
            .expect("host declines the fetch (no backend) → Error event");
        assert!(matches!(err, ClipEventCore::Error { .. }));

        drop(client);
        host.join().unwrap().unwrap();
    }

    fn test_paired_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("punktfunk-paired-test-{}.json", std::process::id()))
    }

    /// Unpaired knock is parked; approve while waiting admits the same connection, no reconnect.
    #[test]
    fn delegated_approval_admits_after_knock() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::NativeClient;
        use punktfunk_core::quic::endpoint;

        let store =
            std::env::temp_dir().join(format!("pf-approval-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&store);
        let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
        let np_host = np.clone();
        let host = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(serve(
                Punktfunk1Options {
                    port: 19779,
                    source: Punktfunk1Source::Synthetic,
                    seconds: 0,
                    frames: 25,
                    max_sessions: 1,
                    max_concurrent: 1,
                    require_pairing: true,
                    allow_pairing: false,
                    pairing_pin: None,
                    paired_store: None,
                    data_port: None,
                    idle_timeout: None,
                    mdns: false,
                },
                0,
                np_host,
                StatsRecorder::new(
                    std::env::temp_dir().join(format!("pf-approval-stats-{}", std::process::id())),
                ),
                crate::identity::ephemeral().unwrap(),
                None,
            ))
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        let (cert, key) = endpoint::generate_identity().unwrap();
        let expected_fp = fingerprint_hex(&endpoint::fingerprint_of_pem(&cert).unwrap());
        let mode = punktfunk_core::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };

        // Approve while the client is still parked.
        let np_approve = np.clone();
        let expect_fp = expected_fp.clone();
        let approver = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            let pend = loop {
                if let Some(p) = np_approve
                    .pending()
                    .into_iter()
                    .find(|p| p.fingerprint == expect_fp)
                {
                    break p;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the knock must register while the client is parked"
                );
                std::thread::sleep(std::time::Duration::from_millis(40));
            };
            assert!(
                pend.name.starts_with("device "),
                "no Hello name → fingerprint-derived label, got {:?}",
                pend.name
            );
            np_approve
                .approve_pending(pend.id, Some("Approved Device"), None)
                .unwrap()
                .paired()
                .expect("pending id must approve");
        });

        // One connect that parks until approved, then streams. Timeout covers park + approver poll.
        let client = NativeClient::connect(
            "127.0.0.1",
            19779,
            mode,
            CompositorPref::Auto,
            GamepadPref::Auto,
            0,
            0,
            2,
            0,
            0,
            None,
            0,
            false,
            None,
            None, // no Hello name — assert the fingerprint-derived label
            None, // TOFU; approval, not a PIN, authorizes this client
            Some((cert, key)),
            std::time::Duration::from_secs(15),
        )
        .expect("approved mid-park → session admitted with no reconnect");
        approver.join().unwrap();
        assert!(
            np.is_paired(&expected_fp),
            "approval must pin the knocking fingerprint"
        );
        assert_eq!(np.list()[0].name, "Approved Device");
        // Hook filters match `client.connected` by name, so it must carry the approval rename.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let connected_name = loop {
            let found = crate::events::bus()
                .subscribe(0)
                .catch_up
                .into_iter()
                .find_map(|e| match e.kind {
                    crate::events::EventKind::ClientConnected { client }
                        if client.fingerprint.as_deref() == Some(expected_fp.as_str()) =>
                    {
                        Some(client.name)
                    }
                    _ => None,
                });
            if let Some(name) = found {
                break name;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "client.connected must fire for the approved device"
            );
            std::thread::sleep(std::time::Duration::from_millis(40));
        };
        assert_eq!(connected_name, "Approved Device");
        drop(client);
        let _ = std::fs::remove_file(&store);
        host.join().unwrap().unwrap();
    }

    /// Right PIN pairs; paired identity gets a session; anonymous does not.
    #[test]
    fn pairing_ceremony_and_gate() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::NativeClient;
        use punktfunk_core::quic::endpoint;

        let host = std::thread::spawn(|| {
            run_ephemeral(Punktfunk1Options {
                port: 19778,
                source: Punktfunk1Source::Synthetic,
                seconds: 0,
                frames: 25,
                max_sessions: 4,
                max_concurrent: 1,
                require_pairing: true,
                allow_pairing: false,
                pairing_pin: Some("4321".into()),
                paired_store: Some(test_paired_path()),
                data_port: None,
                idle_timeout: None,
                mdns: false,
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        let timeout = std::time::Duration::from_secs(10);
        let (cert, key) = endpoint::generate_identity().unwrap();
        let identity = (cert.as_str(), key.as_str());
        let mode = punktfunk_core::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };

        // 1: anonymous session on a pairing-required host → rejected.
        assert!(
            NativeClient::connect(
                "127.0.0.1",
                19778,
                mode,
                CompositorPref::Auto,
                GamepadPref::Auto,
                0,
                0,
                2,
                0,
                0,
                None,
                0,
                false,
                None,
                None,
                None,
                None,
                timeout
            )
            .is_err(),
            "anonymous session must be rejected"
        );

        // 2: correct PIN → paired. The one online attempt consumes the window (step 4).
        let host_fp =
            NativeClient::pair("127.0.0.1", 19778, identity, "4321", "test-client", timeout)
                .expect("pairing with the right PIN");
        assert!(test_paired_path().exists());

        // 3: paired identity gets a session, pinned to the ceremony fingerprint.
        let client = NativeClient::connect(
            "127.0.0.1",
            19778,
            mode,
            CompositorPref::Auto,
            GamepadPref::Auto,
            0,
            0,
            2,
            0,
            0,
            None,
            0,
            false,
            None,
            None,
            Some(host_fp),
            Some((cert.clone(), key.clone())),
            timeout,
        )
        .expect("paired session");
        assert_eq!(client.host_fingerprint, host_fp);
        // Welcome reports a concrete backend. Do not pin which: `PUNKTFUNK_GAMEPAD` may be set.
        assert_ne!(client.resolved_gamepad, GamepadPref::Auto);
        drop(client);

        // 4: single-use PIN — a second attempt (even correct) is rejected.
        std::thread::sleep(PAIRING_COOLDOWN + std::time::Duration::from_millis(200));
        assert!(
            NativeClient::pair("127.0.0.1", 19778, identity, "4321", "too-late", timeout).is_err(),
            "the PIN window must be single-use (one online guess)"
        );
        let _ = std::fs::remove_file(test_paired_path());

        host.join().unwrap().unwrap();
    }

    /// Access clock/threshold arithmetic. The timed task is exercised by the session tests below.
    #[test]
    fn access_deadline_math() {
        let now = 1_700_000_000i64;
        // Wire: 0 = permanent; a due/past deadline still reads as expiring (floor 1).
        assert_eq!(remaining_secs_wire(None, now), 0);
        assert_eq!(remaining_secs_wire(Some(now + 90), now), 90);
        assert_eq!(remaining_secs_wire(Some(now), now), 1);
        assert_eq!(remaining_secs_wire(Some(now - 50), now), 1);

        // Thresholds already behind the deadline are spent, not fired.
        assert_eq!(spent_warnings(None, now), [true, true]);
        assert_eq!(spent_warnings(Some(now + 400), now), [false, false]);
        assert_eq!(spent_warnings(Some(now + 120), now), [true, false]);
        assert_eq!(spent_warnings(Some(now + 30), now), [true, true]);

        // Sleep toward the next unfired boundary, 1..=30 s; permanent parks long.
        assert_eq!(
            access_sleep(None, &[true, true], now),
            std::time::Duration::from_secs(3600)
        );
        // 400 s out, T−5 m unfired → 100 s away, capped at the 30 s NTP-staleness bound.
        assert_eq!(
            access_sleep(Some(now + 400), &[false, false], now),
            std::time::Duration::from_secs(30)
        );
        // 90 s out, only T−1 m left → 30 s away.
        assert_eq!(
            access_sleep(Some(now + 90), &[true, false], now),
            std::time::Duration::from_secs(30)
        );
        // 10 s out, all warned → the deadline itself.
        assert_eq!(
            access_sleep(Some(now + 10), &[true, true], now),
            std::time::Duration::from_secs(10)
        );
        // Due now → 1 s floor (never a busy-spin zero sleep).
        assert_eq!(
            access_sleep(Some(now), &[true, true], now),
            std::time::Duration::from_secs(1)
        );
    }

    /// Controller-only passes pads only; View-only passes nothing. Classify is pinned in core.
    #[test]
    fn input_admission_matrix_and_quiet_drop_accounting() {
        use punktfunk_core::quic::{GRANT_PRESET_CONTROLLER_ONLY, GRANT_PRESET_VIEW_ONLY};
        let admitted = |mask: u32, kind: InputKind| mask & classify(kind).bit() != 0;

        for kind in [
            InputKind::GamepadButton,
            InputKind::GamepadAxis,
            InputKind::GamepadState,
            InputKind::GamepadRemove,
            InputKind::GamepadArrival,
        ] {
            assert!(admitted(GRANT_PRESET_CONTROLLER_ONLY, kind), "{kind:?}");
            assert!(!admitted(GRANT_PRESET_VIEW_ONLY, kind), "{kind:?}");
        }
        for kind in [
            InputKind::KeyDown,
            InputKind::KeyUp,
            InputKind::MouseMove,
            InputKind::MouseMoveAbs,
            InputKind::MouseScroll,
            InputKind::TouchDown,
        ] {
            assert!(!admitted(GRANT_PRESET_CONTROLLER_ONLY, kind), "{kind:?}");
            assert!(!admitted(GRANT_PRESET_VIEW_ONLY, kind), "{kind:?}");
        }
        assert!(admitted(GRANT_ALL, InputKind::KeyDown));

        // Per-class counters; `"none"` when clean.
        let drops = GrantDrops::new();
        assert_eq!(drops.summary(), "none");
        drops.note(GrantClass::Keyboard);
        drops.note(GrantClass::Keyboard);
        drops.note(GrantClass::Mic);
        assert_eq!(drops.summary(), "Keyboard=2 Mic=1");
    }

    /// Pairing-required synthetic host sharing `np` so the test can edit the store live.
    /// Generous `frames`; the typed close cuts the stream.
    fn spawn_access_host(
        port: u16,
        max_sessions: u32,
        np: Arc<NativePairing>,
    ) -> std::thread::JoinHandle<Result<()>> {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(serve(
                Punktfunk1Options {
                    port,
                    source: Punktfunk1Source::Synthetic,
                    seconds: 0,
                    frames: 3000, // ~50 s at 60 fps; the stop flag cuts it long before
                    max_sessions,
                    max_concurrent: 1,
                    require_pairing: true,
                    allow_pairing: false,
                    pairing_pin: None,
                    paired_store: None,
                    data_port: None,
                    idle_timeout: None,
                    mdns: false,
                },
                0,
                np,
                StatsRecorder::new(
                    std::env::temp_dir()
                        .join(format!("pf-access-stats-{port}-{}", std::process::id())),
                ),
                crate::identity::ephemeral().unwrap(),
                None,
            ))
        })
    }

    /// Paired-store temp path; the shared-`np` hosts persist through it.
    fn access_store_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("pf-access-{tag}-{}.json", std::process::id()))
    }

    /// Hello → Welcome → Start, returning streams so a test can read `AccessUpdate`s and the
    /// exact close code. The `UdpSocket` keeps the advertised video port bound.
    async fn raw_session(
        port: u16,
        identity: (&str, &str),
    ) -> (
        quinn::Connection,
        quinn::SendStream,
        quinn::RecvStream,
        Welcome,
        std::net::UdpSocket,
    ) {
        let (ep, _observed) = endpoint::client_pinned_with_identity(None, Some(identity));
        let ep = ep.expect("client endpoint");
        let conn = ep
            .connect(format!("127.0.0.1:{port}").parse().unwrap(), "punktfunk")
            .expect("connect")
            .await
            .expect("QUIC handshake");
        let (mut send, mut recv) = conn.open_bi().await.expect("control stream");
        let hello = Hello {
            abi_version: punktfunk_core::WIRE_VERSION,
            mode: punktfunk_core::Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: Some("access-test".into()),
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            // This fixture is about grants, not audio. Defaults keep Hello byte-identical
            // to a client that omits hi-res fields.
            audio_rate_hz: punktfunk_core::audio::SAMPLE_RATE_HZ,
            audio_bits: punktfunk_core::audio::pcm::BITS_16,
            audio_layout: 0,
            video_fit: 0,
        };
        io::write_msg(&mut send, &hello.encode())
            .await
            .expect("Hello");
        let welcome = Welcome::decode(&io::read_msg(&mut recv).await.expect("Welcome read"))
            .expect("Welcome decode");
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let start = Start {
            client_udp_port: udp.local_addr().unwrap().port(),
        };
        io::write_msg(&mut send, &start.encode())
            .await
            .expect("Start");
        (conn, send, recv, welcome, udp)
    }

    /// Application close code. Panics on a transport-level end — these tests expect a host close.
    async fn closed_app_code(conn: &quinn::Connection) -> u32 {
        match conn.closed().await {
            quinn::ConnectionError::ApplicationClosed(ac) => {
                u32::try_from(u64::from(ac.error_code)).expect("close code fits u32")
            }
            other => panic!("expected an application close, got {other:?}"),
        }
    }

    /// Short expiry: Welcome advertises grants + remaining; deadline closes typed (`0x69`).
    #[test]
    fn access_expiry_advertises_and_closes_typed() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::quic::endpoint;

        let store = access_store_path("expiry");
        let _ = std::fs::remove_file(&store);
        let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
        let (cert, key) = endpoint::generate_identity().unwrap();
        let fp_hex = fingerprint_hex(&endpoint::fingerprint_of_pem(&cert).unwrap());
        np.add_with_access(
            "Evening Guest",
            &fp_hex,
            Some(crate::native_pairing::Access {
                grants: GRANT_ALL,
                expires_unix: Some(wall_unix_now() + 2),
                until_disconnect: false,
            }),
        )
        .unwrap();
        let host = spawn_access_host(19782, 1, np.clone());
        std::thread::sleep(std::time::Duration::from_millis(500));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (conn, _send, _recv, welcome, _udp) =
                raw_session(19782, (cert.as_str(), key.as_str())).await;
            assert_eq!(welcome.grants, GRANT_ALL, "the Welcome advertises the mask");
            assert!(
                (1..=2).contains(&welcome.expires_in_secs),
                "a 2 s grant must advertise 1–2 remaining secs, got {}",
                welcome.expires_in_secs
            );
            let code =
                tokio::time::timeout(std::time::Duration::from_secs(10), closed_app_code(&conn))
                    .await
                    .expect("the deadline task must close the session");
            assert_eq!(
                code,
                punktfunk_core::reject::ACCESS_EXPIRED_CLOSE_CODE,
                "expiry must close with the typed code"
            );
        });
        // The row survives expiry — only authorization ends.
        assert!(np.is_paired(&fp_hex));
        assert_eq!(np.effective(&fp_hex, wall_unix_now()), None);
        let _ = std::fs::remove_file(&store);
        host.join().unwrap().unwrap();
    }

    /// Mid-session grant edit → `AccessUpdate`; T−1 m warning fires; "expire now" typed-closes.
    #[test]
    fn access_edit_pushes_updates_and_expire_now_closes() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::quic::endpoint;

        let store = access_store_path("edit");
        let _ = std::fs::remove_file(&store);
        let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
        let (cert, key) = endpoint::generate_identity().unwrap();
        let fp_hex = fingerprint_hex(&endpoint::fingerprint_of_pem(&cert).unwrap());
        np.add("Edited Device", &fp_hex).unwrap();
        let host = spawn_access_host(19783, 1, np.clone());
        std::thread::sleep(std::time::Duration::from_millis(500));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (conn, _send, mut recv, welcome, _udp) =
                raw_session(19783, (cert.as_str(), key.as_str())).await;
            assert_eq!(welcome.grants, GRANT_ALL);
            assert_eq!(welcome.expires_in_secs, 0, "permanent access advertises 0");

            // Controller-only, 62 s out (inside T−5 m, outside T−1 m): one warning, ~2 s later.
            let now = wall_unix_now();
            np.set_access(
                &fp_hex,
                crate::native_pairing::Access {
                    grants: punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
                    expires_unix: Some(now + 62),
                    until_disconnect: false,
                },
            )
            .unwrap()
            .then_some(())
            .expect("the fingerprint is paired");

            // Update 1: the edit itself (new mask + remaining).
            let msg =
                tokio::time::timeout(std::time::Duration::from_secs(5), io::read_msg(&mut recv))
                    .await
                    .expect("edit AccessUpdate owed")
                    .expect("control stream open");
            let u = AccessUpdate::decode(&msg).expect("an AccessUpdate");
            assert_eq!(u.grants, punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY);
            assert!(
                (55..=62).contains(&u.remaining_secs),
                "remaining should track the fresh deadline, got {}",
                u.remaining_secs
            );

            // Update 2: T−1 m warning, fired as the threshold is crossed live.
            let msg =
                tokio::time::timeout(std::time::Duration::from_secs(10), io::read_msg(&mut recv))
                    .await
                    .expect("T-1m warning owed")
                    .expect("control stream open");
            let u = AccessUpdate::decode(&msg).expect("an AccessUpdate");
            assert!(
                u.remaining_secs <= 60,
                "the warning carries the crossed threshold, got {}",
                u.remaining_secs
            );

            // Expire now: deadline in the past → typed close, no phantom update.
            np.set_access(
                &fp_hex,
                crate::native_pairing::Access {
                    grants: punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
                    expires_unix: Some(wall_unix_now() - 1),
                    until_disconnect: false,
                },
            )
            .unwrap();
            let code =
                tokio::time::timeout(std::time::Duration::from_secs(10), closed_app_code(&conn))
                    .await
                    .expect("expire-now must close the session");
            assert_eq!(code, punktfunk_core::reject::ACCESS_EXPIRED_CLOSE_CODE);
        });
        let _ = std::fs::remove_file(&store);
        host.join().unwrap().unwrap();
    }

    /// Launch without the grant: typed 0x6A before handshake. Same device without launch is admitted.
    #[test]
    fn launch_refused_without_grant_but_session_admitted() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::NativeClient;
        use punktfunk_core::quic::endpoint;

        let store = access_store_path("launch");
        let _ = std::fs::remove_file(&store);
        let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
        let (cert, key) = endpoint::generate_identity().unwrap();
        let fp_hex = fingerprint_hex(&endpoint::fingerprint_of_pem(&cert).unwrap());
        np.add_with_access(
            "Guest Pad",
            &fp_hex,
            Some(crate::native_pairing::Access {
                grants: punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
                expires_unix: None,
                until_disconnect: false,
            }),
        )
        .unwrap();
        // max_sessions counts accepted connections; the refused launch connect is one too.
        let host = spawn_access_host(19784, 2, np.clone());
        std::thread::sleep(std::time::Duration::from_millis(500));
        let timeout = std::time::Duration::from_secs(10);
        let mode = punktfunk_core::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };

        // 1: launch without LAUNCH → typed pre-handshake refusal (`NativeClient` has no Debug).
        let refused = NativeClient::connect(
            "127.0.0.1",
            19784,
            mode,
            CompositorPref::Auto,
            GamepadPref::Auto,
            0,
            0,
            2,
            0,
            0,
            None,
            0,
            false,
            Some("steam:570".into()),
            Some("Guest Pad".into()),
            None,
            Some((cert.clone(), key.clone())),
            timeout,
        );
        match refused {
            Ok(_) => panic!("a launch without the grant must be refused"),
            Err(punktfunk_core::PunktfunkError::Rejected(r)) => assert_eq!(
                r,
                punktfunk_core::reject::RejectReason::LaunchNotPermitted,
                "the refusal must carry the typed launch reason"
            ),
            Err(other) => panic!("expected a typed rejection, got {other:?}"),
        }

        // 2: same device without a launch is admitted.
        let client = NativeClient::connect(
            "127.0.0.1",
            19784,
            mode,
            CompositorPref::Auto,
            GamepadPref::Auto,
            0,
            0,
            2,
            0,
            0,
            None,
            0,
            false,
            None,
            Some("Guest Pad".into()),
            None,
            Some((cert, key)),
            timeout,
        )
        .expect("controller-only session without a launch must be admitted");
        drop(client);
        let _ = std::fs::remove_file(&store);
        host.join().unwrap().unwrap();
    }

    /// A launch the host cannot resolve streams on, and the client learns why from the
    /// control message.
    #[test]
    fn unknown_launch_reaches_the_client_as_a_refusal() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::NativeClient;
        use punktfunk_core::quic::{endpoint, LaunchOutcomeKind};

        let store = access_store_path("launch-outcome");
        let _ = std::fs::remove_file(&store);
        let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
        let (cert, key) = endpoint::generate_identity().unwrap();
        let fp_hex = fingerprint_hex(&endpoint::fingerprint_of_pem(&cert).unwrap());
        np.add_with_access("Launcher", &fp_hex, None).unwrap();
        let host = spawn_access_host(19786, 1, np);
        std::thread::sleep(std::time::Duration::from_millis(500));
        let client = NativeClient::connect(
            "127.0.0.1",
            19786,
            punktfunk_core::Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            CompositorPref::Auto,
            GamepadPref::Auto,
            0,
            0,
            2,
            0,
            0,
            None,
            0,
            false,
            Some("pf-test:no-such-title".into()),
            Some("Launcher".into()),
            None,
            Some((cert, key)),
            std::time::Duration::from_secs(10),
        )
        .expect("an unresolvable launch still admits the session");
        // A cold library scan decides the refusal; it can take seconds.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let outcome = loop {
            if let Some(o) = client.launch_outcome() {
                break o;
            }
            assert!(std::time::Instant::now() < deadline, "no launch outcome");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(outcome.kind, LaunchOutcomeKind::Refused);
        assert!(outcome
            .notice()
            .is_some_and(|n| n.starts_with("Couldn't start")));
        drop(client);
        let _ = std::fs::remove_file(&store);
        host.join().unwrap().unwrap();
    }

    /// Expired record knocks into pending; re-approval is the re-grant on the held connection.
    #[test]
    fn expired_record_knocks_into_pending_and_reapproval_regrants() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::NativeClient;
        use punktfunk_core::quic::endpoint;

        let store = access_store_path("regrant");
        let _ = std::fs::remove_file(&store);
        let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
        let (cert, key) = endpoint::generate_identity().unwrap();
        let fp_hex = fingerprint_hex(&endpoint::fingerprint_of_pem(&cert).unwrap());
        // Still listed, no longer authorized.
        np.add_with_access(
            "Yesterday's Guest",
            &fp_hex,
            Some(crate::native_pairing::Access {
                grants: GRANT_ALL,
                expires_unix: Some(wall_unix_now() - 3600),
                until_disconnect: false,
            }),
        )
        .unwrap();
        assert!(np.is_paired(&fp_hex), "expired but still listed");
        assert_eq!(np.effective(&fp_hex, wall_unix_now()), None);

        let host = spawn_access_host(19785, 1, np.clone());
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Reconnect appears as pending; approve with fresh access while parked.
        let np_approve = np.clone();
        let fp_approve = fp_hex.clone();
        let approver = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            let pend = loop {
                if let Some(p) = np_approve
                    .pending()
                    .into_iter()
                    .find(|p| p.fingerprint == fp_approve)
                {
                    break p;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "an expired record's reconnect must knock into the pending list"
                );
                std::thread::sleep(std::time::Duration::from_millis(40));
            };
            np_approve
                .approve_pending(
                    pend.id,
                    None,
                    Some(crate::native_pairing::Access {
                        grants: punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
                        expires_unix: Some(wall_unix_now() + 4 * 3600),
                        until_disconnect: false,
                    }),
                )
                .unwrap()
                .paired()
                .expect("re-approval");
        });

        let client = NativeClient::connect(
            "127.0.0.1",
            19785,
            punktfunk_core::Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            CompositorPref::Auto,
            GamepadPref::Auto,
            0,
            0,
            2,
            0,
            0,
            None,
            0,
            false,
            None,
            Some("Yesterday's Guest".into()),
            None,
            Some((cert, key)),
            std::time::Duration::from_secs(15),
        )
        .expect("re-approved mid-park → session admitted with no reconnect");
        approver.join().unwrap();
        // Re-grant in force: controller-only.
        assert_eq!(
            np.effective(&fp_hex, wall_unix_now()),
            Some(punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY)
        );
        drop(client);
        let _ = std::fs::remove_file(&store);
        host.join().unwrap().unwrap();
    }
}
