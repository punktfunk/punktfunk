//! Native `punktfunk/1` host: QUIC control plane plus the core data plane over UDP.
//!
//! Welcome negotiates GF(2¹⁶) Leopard FEC and AES-GCM. Hello names a display mode;
//! the host opens a virtual output at that size/refresh (same backends as GameStream).
//! Input arrives as QUIC datagrams into the session injector. Video AUs carry
//! wall-clock `pts_ns`. Concurrent sessions share host-lifetime audio/input/mic;
//! isolated gamescope spawns do not. Data plane is native threads, not async.
//! A session also carries desktop Opus (`AUDIO_MAGIC`) and gamepads (`RUMBLE_MAGIC`).
//!
//! Serves `~/.config/punktfunk/native-cert.pem` (shared with the mgmt API) and logs
//! the SHA-256 fingerprint clients pin. `punktfunk-probe --connect host:9777` is
//! the counterpart. Evidence: `design/` and `native/tests.rs`.

use anyhow::{anyhow, Context, Result};
use punktfunk_core::config::{CompositorPref, FecConfig, FecScheme, GamepadPref, Role};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::packet::{FLAG_PIC, FLAG_PROBE, FLAG_SOF};
use punktfunk_core::quic::v2::hello::{ClientHello, Ready, ServerHello};
use punktfunk_core::quic::v2::msg as v2msg;
use punktfunk_core::quic::{
    classify, endpoint, pkf1, AccessUpdate, AckReason, BitrateChanged, ClockEcho, ClockProbe,
    ColorInfo, GrantClass, Hello, PairRequest, PipelineGap, ProbeResult, ProbeShaped, Reconfigure,
    Reconfigured, SetBitrate, Welcome, GRANT_ALL, GRANT_CLIPBOARD, GRANT_LAUNCH,
};
use punktfunk_core::session::test_frame;
use punktfunk_core::Session;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

// The session's control connection, whichever transport carries it (quinn or WebTransport).
pub(crate) mod link;
/// A seat's Steam, up before its client asks (`design/steam-seats-warm-launch-implementation-plan.md`).
#[cfg(target_os = "linux")]
pub(crate) mod prewarm;
use crate::compositor_route::{resolve_compositor, GamescopeHold};

/// GameStream presents the same virtual pad and must pick `windows_xbox_hid` from this definition.
pub(crate) mod gamepad;
use gamepad::{resolve_gamepad, resolve_pad_kind, route_decision};

mod pairing;
pub(crate) use pairing::{pair_ceremony, PairWire};

/// The session's expiry clock and live grant watch.
mod access;
use access::{access_lifecycle, remaining_secs_wire};

mod audio;
use audio::audio_thread;

/// Bitrate and FEC policy: resolved rates, the PyroWave pin, the encoder ceiling, adaptive FEC.
mod bitrate;
use bitrate::{adaptive_fec_for, audio_reserved_kbps, fec_static_override};

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
pub(crate) use handshake::redirect;
/// `PUNKTFUNK_WIRE_MTU`, the control-connection path-MTU watch, and the per-peer shard clamp.
mod wire_mtu;

mod control;
mod cursor_fwd;

mod stream;
use stream::{
    reconfig_allowed, software_stream, synthetic_abr_stream, synthetic_stream, virtual_stream,
    SessionContext, StreamCommon, SynthAbrContext,
};
mod wiring;
pub use stream::{Content, KeyframeAnswer, SynthAbrShape, DEFAULT_IDR_PCT};
use wiring::SessionWiring;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Punktfunk1Source {
    /// Protocol-test frames; the client byte-checks the payload.
    Synthetic,
    /// Frames sized from the live wire budget, on the real paced send path. No display and no
    /// GPU: what the netem rig streams so Automatic can be judged on a shaped link. The
    /// [`SynthAbrShape`] is what it encodes, how long it holds the first frame back, and
    /// what it answers a keyframe ask with.
    SyntheticAbr(SynthAbrShape),
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
    /// Disconnect-detection latency. `None` = core default (8 s). From
    /// `PUNKTFUNK_IDLE_TIMEOUT_MS`; ≥1 s floor, keep-alive scales so a live session
    /// never false-closes.
    pub idle_timeout: Option<std::time::Duration>,
    /// `_punktfunk._udp` advert. `--no-mdns` / `PUNKTFUNK_MDNS=0` skips it.
    pub mdns: bool,
}

use crate::native_pairing::{NativePairing, PairingDecision};
use crate::send_pacing::{percentile, PaceStat};
use crate::stats_recorder::StatsRecorder;

/// Bounds online PIN guessing: SPAKE2 already gives one guess per ceremony; this caps the rate.
pub(crate) const PAIRING_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(2);

use punktfunk_core::quic::wall_clock_ns as now_ns;

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
    let profiles = crate::profiles::Profiles::load_with(None, None);
    if let Err(e) = profiles.ensure_owner(&crate::host::machine_hostname()) {
        tracing::warn!(error = %format!("{e:#}"), "owner profile not created");
    }
    // No management API → advertise no `mgmt` port (0).
    rt.block_on(serve(opts, 0, np, Arc::new(profiles), stats, ident, None))
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
    rt.block_on(serve(opts, 0, np, test_profiles(), stats, ident, None))
}

/// A profile store in a temp file: tests never read or write the real `profiles.json`.
#[cfg(test)]
pub(crate) fn test_profiles() -> Arc<crate::profiles::Profiles> {
    use std::sync::atomic::AtomicUsize;
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("pf-profiles-{}-{n}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    Arc::new(crate::profiles::Profiles::load_with(Some(path), None))
}

/// Native host config when unified `serve` runs it in-process.
pub(crate) struct NativeServe {
    pub port: u16,
    /// Default on. `serve --open` turns it off. Pairing is armed on demand from the console.
    pub require_pairing: bool,
    /// Management API TCP port, advertised over mDNS so a client browses the library on this IP.
    pub mgmt_port: u16,
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
        idle_timeout: idle_timeout_from_env(),
        mdns: cfg.mdns,
    }
}

pub(crate) async fn serve(
    opts: Punktfunk1Options,
    mgmt_port: u16,
    np: Arc<NativePairing>,
    profiles: Arc<crate::profiles::Profiles>,
    stats: Arc<StatsRecorder>,
    // Caller-resolved so the planes cannot race the first-run mint.
    identity: crate::identity::NativeIdentity,
    // The browser plane, when the operator asked for it. Spawned from here, not joined: a browser
    // runs this plane's session on this plane's capturer, injector and session pool.
    web: Option<crate::webtransport::Plane>,
) -> Result<()> {
    let fingerprint = endpoint::fingerprint_of_pem(&identity.cert_pem)
        .map_err(|e| anyhow!("cert fingerprint: {e}"))?;
    // Media leaves from this socket. `pkf1` stays listed for [`serve_pkf1`]: an older client
    // still pairs there, and is told to update for anything else.
    let alpns: &[&[u8]] = &[
        punktfunk_core::quic::v2::registry::ALPN,
        endpoint::QUIC_ALPN,
    ];
    let (ep, media_socket) = endpoint::server_shared(
        ([0, 0, 0, 0], opts.port).into(),
        &identity.cert_pem,
        &identity.key_pem,
        opts.idle_timeout.unwrap_or(endpoint::DEFAULT_IDLE_TIMEOUT),
        alpns,
    )
    .map_err(|e| anyhow!("QUIC server endpoint: {e}"))?;
    let media_socket = Arc::new(media_socket);
    tracing::info!(
        port = opts.port,
        source = ?opts.source,
        fingerprint = %hex::encode(fingerprint),
        "punktfunk host listening (QUIC) — clients pin this fingerprint"
    );

    // Held for the host lifetime — dropping `_advert` unregisters. Best-effort: a
    // discovery failure must not stop streaming (`--connect HOST:PORT` still works).
    let _advert = if !opts.mdns {
        tracing::info!(
            "mDNS advertisement disabled (--no-mdns / PUNKTFUNK_MDNS) — clients connect by address"
        );
        None
    } else {
        match crate::host::Host::detect() {
        Ok(h) => crate::discovery::advertise_native(
            &h.hostname,
            opts.port,
            &hex::encode(fingerprint),
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

    // The door places every connect and runs no session, so it opens no audio, input or display
    // and the senders below reach nobody.
    let door = pf_paths::seat::is_door();
    // A sinkless capturer handed session to session (`AudioCapSlot`, `park_audio_capture`).
    let audio_cap: AudioCapSlot = Arc::new(std::sync::Mutex::new(None));
    // Host-lifetime injector: one RemoteDesktop-portal grant. A CreateSession per session
    // races portal teardown on reconnect and wedges KWin EIS. Gamepads stay per-session.
    let injector = (!door).then(crate::inject::InjectorService::start);
    let inj_sender = || {
        injector
            .as_ref()
            .map_or_else(|| std::sync::mpsc::channel().0, |i| i.sender())
    };
    // A crashed host's claims left the box's audio defaults on its own nodes. Off-thread: a
    // sick PipeWire must not hold up serving; a session's claim waits on the same lock.
    if !door {
        std::thread::spawn(crate::audio::heal_audio_defaults);
    }
    // Host-lifetime virtual mic ([`crate::audio::MicPump`]): 0xCB Opus → a persistent source
    // games can bind before they launch. Opens eagerly; self-heals if the backend dies.
    let mic_service = (!door).then(crate::audio::MicPump::start);
    let mic_sender = || {
        mic_service
            .as_ref()
            .map_or_else(|| std::sync::mpsc::sync_channel(1).0, |m| m.sender())
    };
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
    let _restore_worker = (!door).then(crate::vdisplay::start_restore_worker);
    if !door {
        // Recover a takeover stranded by a crashed previous instance (`$XDG_RUNTIME_DIR`).
        crate::vdisplay::restore_takeover_on_startup();
        // Takeover needs the host user in `punktfunk`. Missing membership degrades to mirroring.
        // No-op off Linux.
        crate::vdisplay::preflight_takeover_privilege();
        // Console registry after the probed subsystems are up, so a probe never names a node
        // that was about to appear.
        crate::diagnostics::preflight();
        install_shutdown_restore();
    }
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
            inj_tx: inj_sender(),
            mic_tx: mic_sender(),
            np: np.clone(),
            profiles: profiles.clone(),
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
    // Once the host serves: a seat's Steam takes half a minute to boot, and the point is that it
    // has already booted when its device connects.
    #[cfg(target_os = "linux")]
    if !door {
        prewarm::spawn_run("host start");
    }

    loop {
        // A finished task stays in the set until joined; a serving host never reaches the drain
        // below, and every client probe is a task.
        while sessions.try_join_next().is_some() {}
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
        let profiles = profiles.clone();
        let last_pairing = last_pairing.clone();
        let stats = stats.clone();
        let inj_tx = inj_sender();
        let mic_tx = mic_sender();
        let sem = sem.clone();
        let accepted = accepted.clone();
        let done = done.clone();
        let media_socket = media_socket.clone();
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
            let peer = conn.remote_address();
            if endpoint::negotiated_alpn(&conn).as_deref()
                != Some(punktfunk_core::quic::v2::registry::ALPN)
            {
                serve_pkf1(conn, media_socket, &fingerprint, &np, &last_pairing).await;
                return;
            }
            tracing::info!(%peer, "client connected");
            // `serve_session` takes the slot once the peer has spoken: released while a knock is
            // parked, re-acquired on approval. A setup failure still needs a typed close.
            let sem_session = sem;
            let conn_err = conn.clone();
            let link = link::SessionLink::QuicV2(
                conn.clone(),
                Arc::new(link::V2Link::new(conn, media_socket)),
            );
            match serve_session(
                link,
                &opts,
                &audio_cap,
                inj_tx,
                mic_tx,
                &fingerprint,
                &np,
                &profiles,
                &last_pairing,
                stats,
                sem_session,
            )
            .await
            {
                Ok(Served::Session) => tracing::info!(%peer, "session complete"),
                Ok(Served::ProbeClose) => tracing::debug!(
                    %peer,
                    "closed before the control handshake (reachability probe)"
                ),
                Ok(Served::Management) => tracing::debug!(%peer, "management connection closed"),
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
            // After `serve_session` returns: the stream thread is joined and this session's
            // display lease is gone, so a pre-warm can adopt or replace what it left.
            #[cfg(target_os = "linux")]
            if !door {
                prewarm::spawn_run("session end");
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

/// Catch `SIGTERM`/`SIGINT`, give the box back, then exit. `exit(0)` runs no destructor, so
/// this is where the audio defaults, every display's topology restore and output, and a Game
/// Mode takeover are undone. Crash-restore lives in `$XDG_RUNTIME_DIR`, which logind removes
/// with the user manager. Blocking, under [`SHUTDOWN_RESTORE_GRACE`].
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
        let restore = tokio::task::spawn_blocking(|| {
            crate::audio::restore_audio_defaults();
            // Monitors come back before the outputs go, and before the slower Game Mode restart.
            crate::vdisplay::registry::teardown_all();
            crate::vdisplay::restore_takeover_now();
        });
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
/// Ends the session when the console removes the profile it plays as.
fn spawn_profile_watch(conn: link::SessionLink, profile: String) {
    let mut removed = crate::profiles::removed();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = conn.closed() => return,
                id = removed.recv() => match id {
                    Ok(id) if id == profile => break,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                },
            }
        }
        tracing::info!(%profile, "profile removed — closing its session");
        let reason = punktfunk_core::reject::RejectReason::SeatUnavailable;
        conn.refuse(
            reason.close_code(),
            "Your profile was removed by the host's owner.",
        )
        .await;
    });
}

async fn close_rejected(conn: &link::SessionLink, reason: punktfunk_core::reject::RejectReason) {
    conn.refuse(reason.close_code(), &reason.to_string()).await;
}

/// Client close code for a deliberate quit (user "stop"). Tears the virtual display down
/// immediately, skipping the keep-alive linger. Any other close still lingers for reconnect.
const QUIT_CODE: u32 = punktfunk_core::quic::QUIT_CLOSE_CODE;

/// 2 / 6 / 8; anything else (older client, garbage) becomes stereo. Both backends can
/// produce the count; fewer real sink channels just carry up/downmixed content.
fn resolve_audio_channels(requested: u8) -> u8 {
    punktfunk_core::audio::normalize_channels(requested)
}

use crate::audio::AudioCapSlot;

/// Park an unpaired knock for console Approve. QUIC keep-alive (4 s, under 8 s idle) holds
/// the path; approval streams with no reconnect. Under the pending TTL (10 min).
const PENDING_APPROVAL_WAIT: std::time::Duration = std::time::Duration::from_secs(180);

/// How often a parked `punktfunk/2` knock is told the host is still deciding.
const PENDING_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// Park an unpaired knock until the console decides. The caller holds no session slot while
/// it waits. A `punktfunk/2` client hears `Pending` on `v2` meanwhile, every [`PENDING_EVERY`].
///
/// `Ok(Ok(_))` is an approval, with a slot taken like any fresh client's (waits if busy).
/// `Ok(Err(reason))` is the refusal to send. `Err` means the client left before a decision.
pub(crate) async fn park_knock(
    conn: &link::SessionLink,
    mut send: Option<&mut link::CtlSend>,
    np: &NativePairing,
    label: &str,
    fp_hex: &str,
    profile: Option<&str>,
    sem: &Arc<tokio::sync::Semaphore>,
) -> Result<Result<tokio::sync::OwnedSemaphorePermit, punktfunk_core::reject::RejectReason>> {
    use punktfunk_core::reject::RejectReason;
    if np.pairing_refused() {
        tracing::info!(name = %label, fingerprint = %fp_hex,
            "unpaired device knocked on a seat — it pairs with the box");
        return Ok(Err(RejectReason::PairingNotArmed));
    }
    tracing::info!(name = %label, fingerprint = %fp_hex,
        "unpaired device knocked — parking connection for delegated approval in the console");
    // QUIC-validated source IP for the pending per-source cap. Knock generation makes
    // this connection the one an approval admits — siblings must not all start a session.
    let knock_seq = np.note_pending(label, fp_hex, Some(conn.remote_address().ip()), profile);
    let wait = np.wait_for_decision(fp_hex, knock_seq, PENDING_APPROVAL_WAIT);
    tokio::pin!(wait);
    let mut pending = tokio::time::interval(PENDING_EVERY);
    let decision = loop {
        tokio::select! {
            d = &mut wait => break d,
            _ = conn.closed() => anyhow::bail!("client disconnected before pairing approval"),
            _ = pending.tick() => {
                if let Some(w) = send.as_deref_mut() {
                    let _ = punktfunk_core::quic::v2::io::send(w, &v2msg::Pending {}).await;
                }
            }
        }
    };
    let reason = match decision {
        PairingDecision::Approved => {
            tracing::info!(name = %label, fingerprint = %fp_hex,
                "device approved in console — admitting session (no reconnect)");
            let permit = sem.clone().acquire_owned().await;
            return Ok(Ok(permit.expect("session semaphore is never closed")));
        }
        PairingDecision::Denied => RejectReason::Denied,
        // The device can knock again.
        PairingDecision::TimedOut => RejectReason::ApprovalTimeout,
        // Only the newest connection from a device is admitted on approval.
        PairingDecision::Superseded => RejectReason::Superseded,
    };
    Ok(Err(reason))
}

/// A QUIC handshake that closes code 0 with no control stream is a reachability probe
/// (`--reachable` / hosts-page pips). Log at debug, not warn.
pub(crate) enum Served {
    Session,
    ProbeClose,
    Management,
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
    if let Some(m) = e.downcast_ref::<pf_vdisplay::monitors::MonitorNotFound>() {
        return Some(m.user_message());
    }
    e.downcast_ref::<pf_vdisplay::DisplayAsleep>()
        .map(|d| d.user_message())
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
    profiles: &Arc<crate::profiles::Profiles>,
    last_pairing: &std::sync::Mutex<Option<std::time::Instant>>,
    stats: Arc<StatsRecorder>,
    // The session slots. An unpaired knock releases its slot while parked, re-acquires on approval.
    sem: Arc<tokio::sync::Semaphore>,
) -> Result<Served> {
    let np: &NativePairing = np_arc;
    let (mut send, mut recv) = match tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| anyhow!("control stream timeout"))??
    {
        // Clean close before any control stream: reachability probe ([`Served::ProbeClose`]).
        link::Accepted::ProbeClose => return Ok(Served::ProbeClose),
        // Before the session slot: a management connection streams nothing.
        link::Accepted::Management(send, recv) => {
            let ip = conn
                .local_ip()
                .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED.into());
            let local = std::net::SocketAddr::new(ip, opts.port);
            crate::webtransport::mgmt::serve_quic(conn.quic().clone(), (send, recv), local).await?;
            return Ok(Served::Management);
        }
        link::Accepted::Stream(send, recv) => (send, link::CtlReader::new(recv)),
    };
    let (ty, body) = tokio::time::timeout(HANDSHAKE_TIMEOUT, recv.read_frame())
        .await
        .map_err(|_| anyhow!("first message timeout"))??;
    if let Ok(req) = v2msg::decode::<PairRequest>(ty, &body) {
        let wire = pairing::PairWire::V2 { send, recv };
        return serve_pairing(&conn, wire, req, host_fp, np, last_pairing).await;
    }
    let first = v2msg::decode::<ClientHello>(ty, &body)
        .map_err(|e| anyhow!("ClientHello decode: {e:?}"))?;

    // A slot only once the peer has spoken: one that stalls the handshake holds none, so it
    // cannot queue paired clients behind it. A full host still accepts, so the waiter sees a
    // live path (keep-alive) instead of a silent dial timeout.
    let mut permit = sem
        .clone()
        .acquire_owned()
        .await
        .expect("session semaphore is never closed");
    // Pairing gate outside the handshake future: approval wait must not be bound by
    // HANDSHAKE_TIMEOUT, and the NVENC permit is released while parked.
    if opts.require_pairing {
        let gate_hello = &first.hello;
        let fp = conn.peer_fingerprint();
        // `effective`, not `is_paired`: an expired record is listed but not authorized, so it
        // knocks like an unpaired device and re-approval is the re-grant.
        let authorized = fp
            .as_ref()
            .map(|fp| {
                np.effective(&hex::encode(fp), crate::clock::unix_secs())
                    .is_some()
            })
            .unwrap_or(false);
        if !authorized {
            // Anonymous: no identity to approve. PIN ceremony is the way in.
            let Some(fp) = fp else {
                close_rejected(
                    &conn,
                    punktfunk_core::reject::RejectReason::IdentityRequired,
                )
                .await;
                anyhow::bail!(
                    "unpaired anonymous client rejected (this host requires pairing — present a \
                     client identity and approve it in the console, or run the PIN ceremony)"
                );
            };
            let fp_hex = hex::encode(fp);
            // Sanitize the wire name before log/console (escapes / bidi). Empty → fingerprint label.
            let label = crate::native_pairing::sanitize_device_name(
                gate_hello.name.as_deref().unwrap_or(""),
                &fp_hex,
            );
            drop(permit);
            let asked = first.profile.as_deref();
            permit =
                match park_knock(&conn, Some(&mut send), np, &label, &fp_hex, asked, &sem).await? {
                    Ok(permit) => permit,
                    Err(reason) => {
                        close_rejected(&conn, reason).await;
                        anyhow::bail!("pairing request refused: {reason}");
                    }
                };
        }
    }
    // Admitted. From here the session is the same on every carrier.
    let host = SessionHost {
        opts: Arc::clone(opts),
        audio_cap: audio_cap.clone(),
        inj_tx,
        mic_tx,
        np: np_arc.clone(),
        profiles: profiles.clone(),
        stats,
    };
    let data_plane = DataPlane::Shared(
        conn.v2()
            .context("the native plane admitted a link with no punktfunk/2 state")?
            .clone(),
    );
    run_admitted(conn, send, recv, first, &host, data_plane, permit).await
}

/// A `PairRequest`: the PIN gate, then the ceremony. Pairing is its own connection; no session
/// follows on it.
async fn serve_pairing<W, R>(
    conn: &link::SessionLink,
    wire: pairing::PairWire<W, R>,
    req: PairRequest,
    host_fp: &[u8; 32],
    np: &NativePairing,
    last_pairing: &std::sync::Mutex<Option<std::time::Instant>>,
) -> Result<Served>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    let peer = conn.remote_address();
    // Fingerprint-bound PIN window: only this device may consume (or burn) it.
    let Some(client_fp) = conn.peer_fingerprint() else {
        close_rejected(conn, punktfunk_core::reject::RejectReason::IdentityRequired).await;
        anyhow::bail!("pairing requires the client to present a certificate");
    };
    let client_fp_hex = hex::encode(client_fp);
    // Charge the cooldown before consulting arming, on every outcome including rejections.
    // Otherwise "is pairing armed?" is a free oracle. A spam of knocks can hold the
    // cooldown against the real device.
    let limited = {
        let mut last = last_pairing.lock().unwrap();
        let limited = last.is_some_and(|t| t.elapsed() < PAIRING_COOLDOWN);
        if !limited {
            *last = Some(std::time::Instant::now());
        }
        limited
    };
    if limited {
        close_rejected(
            conn,
            punktfunk_core::reject::RejectReason::PairingRateLimited,
        )
        .await;
        anyhow::bail!("pairing rate-limited — retry shortly");
    }
    // Live PIN per attempt so a lapsed window no longer pairs; honor fingerprint binding
    // and the address the knock came from.
    let source = crate::native_pairing::classify_source(Some(peer.ip()));
    let pin = match np.pin_for_attempt(&client_fp_hex, source) {
        crate::native_pairing::PinAttempt::Pin(pin) => pin,
        crate::native_pairing::PinAttempt::Disarmed => {
            close_rejected(conn, punktfunk_core::reject::RejectReason::PairingNotArmed).await;
            anyhow::bail!(
                "pairing not armed (arm it in the console, or start with --allow-pairing)"
            )
        }
        // Armed for a different device: reject without the ceremony so this does not burn the window.
        crate::native_pairing::PinAttempt::BoundToOther => {
            close_rejected(
                conn,
                punktfunk_core::reject::RejectReason::PairingBoundToOtherDevice,
            )
            .await;
            anyhow::bail!(
                "pairing is armed for a different device — this attempt does not consume the window"
            )
        }
        // An open window is for the device in the operator's hands. This one is on the
        // internet, so it reads as not armed and the window survives for its owner.
        crate::native_pairing::PinAttempt::UnboundForWan => {
            close_rejected(conn, punktfunk_core::reject::RejectReason::PairingNotArmed).await;
            anyhow::bail!(
                "a knock from {peer} needs a pairing window bound to its fingerprint \
                 ({client_fp_hex}) — an open window does not answer the internet"
            )
        }
    };
    pair_ceremony(conn, wire, req, &client_fp, host_fp, np, &pin)
        .await
        .map(|()| Served::Session)
}

/// A `pkf1` dial. A client that pairs over `pkf1` still reaches the PIN ceremony, in that
/// wire's framing; every other `pkf1` dial is closed with the wire-version code, which the
/// client shows as "update both".
async fn serve_pkf1(
    conn: quinn::Connection,
    media_socket: Arc<std::net::UdpSocket>,
    host_fp: &[u8; 32],
    np: &NativePairing,
    last_pairing: &std::sync::Mutex<Option<std::time::Instant>>,
) {
    let peer = conn.remote_address();
    let first = async {
        let (send, mut recv) = conn.accept_bi().await?;
        let first = pkf1::read(&mut recv).await?;
        anyhow::Ok((send, recv, first))
    };
    if let Ok(Ok((send, recv, first))) = tokio::time::timeout(HANDSHAKE_TIMEOUT, first).await {
        if let Ok(req) = PairRequest::decode_pkf1(&first) {
            let link = link::SessionLink::QuicV2(
                conn.clone(),
                Arc::new(link::V2Link::new(conn.clone(), media_socket)),
            );
            let wire = pairing::PairWire::Pkf1 { send, recv };
            match serve_pairing(&link, wire, req, host_fp, np, last_pairing).await {
                Ok(_) => tracing::info!(%peer, "pairing over punktfunk/1 complete"),
                Err(e) => {
                    tracing::warn!(%peer, error = %format!("{e:#}"), "pairing over punktfunk/1 failed")
                }
            }
            return;
        }
    }
    let reason = punktfunk_core::reject::RejectReason::WireVersionMismatch;
    conn.close(reason.close_code().into(), reason.to_string().as_bytes());
    tracing::info!(%peer, "punktfunk/1 client refused — it needs an update");
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
    /// Resolves each connect's profile before anything is built for it.
    pub(crate) profiles: Arc<crate::profiles::Profiles>,
    pub(crate) stats: Arc<StatsRecorder>,
}

impl SessionHost {
    /// A host with nothing behind it: every channel's receiver is dropped. For tests of the
    /// admission code, which never reach the pipeline.
    #[cfg(test)]
    pub(crate) fn for_tests(
        np: Arc<NativePairing>,
        profiles: Arc<crate::profiles::Profiles>,
    ) -> SessionHost {
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
                idle_timeout: None,
                mdns: false,
            }),
            audio_cap: Arc::new(std::sync::Mutex::new(None)),
            inj_tx: std::sync::mpsc::channel().0,
            mic_tx: std::sync::mpsc::sync_channel(1).0,
            np,
            profiles,
            stats: StatsRecorder::new(crate::stats_recorder::default_dir()),
        }
    }
}

/// Where video goes: the native endpoint's own socket, toward the connection's validated
/// address, or a browser's WebTransport datagrams. The stream thread turns either into the
/// `Box<dyn Transport>` that `Session` is written against.
pub(crate) enum DataPlane {
    Web(crate::webtransport::WebTransportPlane),
    Shared(Arc<link::V2Link>),
}

/// The session proper, after admission. Carrier-agnostic: the control stream is a [`link::CtlSend`]
/// / [`link::CtlRecv`] pair, datagrams go through [`link::SessionLink`], and video through
/// whatever [`DataPlane`] the caller built. The device is the link's
/// [`link::SessionLink::peer_fingerprint`], whichever plane admitted it.
pub(crate) async fn run_admitted(
    conn: link::SessionLink,
    send: link::CtlSend,
    recv: link::CtlReader,
    first: ClientHello,
    host: &SessionHost,
    data_plane: DataPlane,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Served> {
    let session_fp_hex = conn.peer_fingerprint().map(hex::encode);
    let SessionHost {
        opts,
        audio_cap,
        inj_tx,
        mic_tx,
        np,
        profiles,
        stats,
    } = host;
    let (opts, np, stats) = (opts.as_ref(), np.as_ref(), stats.clone());
    let (inj_tx, mic_tx) = (inj_tx.clone(), mic_tx.clone());
    let (mut send, mut recv) = (send, recv);
    let peer = conn.remote_address();
    // RAII frees the slot on return.
    let _permit = permit;

    let Admission {
        grants: initial_grants,
        deadline_unix,
        watch: access_watch,
        at_unix: admit_unix,
        session: _session,
    } = admit(host, session_fp_hex.as_deref(), &conn, &first).await?;
    // Before anything is built for it: an unknown profile closes with no display touched.
    let resolved = match profiles.resolve(session_fp_hex.as_deref(), first.profile.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            use punktfunk_core::reject::RejectReason;
            let reason = match e {
                crate::profiles::ProfileError::Unknown
                | crate::profiles::ProfileError::NotThisSeat => RejectReason::ProfileUnknown,
                crate::profiles::ProfileError::SessionUnavailable => RejectReason::SeatUnavailable,
            };
            close_rejected(&conn, reason).await;
            anyhow::bail!("profile refused: {e:?}");
        }
    };
    tracing::info!(profile = %resolved.id, name = %resolved.display_name, via = ?resolved.via, "profile");
    profiles.touch(&resolved.id);
    // A seat profile plays on its seat's host: send the client there, or say why not.
    {
        use crate::seats::placement::{place, Asker, Placement};
        let fp = session_fp_hex.clone();
        let follows = first
            .features
            .has(punktfunk_core::quic::v2::registry::FEATURE_PROFILES);
        let who = resolved.clone();
        let placed = tokio::task::spawn_blocking(move || {
            let joins = crate::vdisplay::policy::prefs()
                .get()
                .effective_for(fp.as_deref())
                .mode_conflict
                == crate::vdisplay::policy::ModeConflict::Join;
            let asker = Asker {
                fp: fp.as_deref(),
                follows_redirects: follows,
                joins,
            };
            place(&who, &asker)
        })
        .await
        .context("placement task")?;
        match placed {
            Placement::Here => {}
            Placement::Redirect(to) => {
                tracing::info!(profile = %resolved.id, seat = %to.seat_name, port = to.port,
                    "redirected to the profile's seat");
                redirect(&conn, &mut send, &to).await?;
                return Ok(Served::Session);
            }
            Placement::Refuse(reason) => {
                close_rejected(&conn, reason).await;
                anyhow::bail!("seat refused: {reason}");
            }
        }
    }
    spawn_profile_watch(conn.clone(), resolved.id.clone());
    let profile_ref = crate::events::ProfileRef {
        id: resolved.id.clone(),
        display_name: resolved.display_name.clone(),
    };
    // One relaxed load per event; the lifecycle task is the only writer after admission.
    let session_grants = Arc::new(AtomicU32::new(initial_grants));
    let expires_in_secs = remaining_secs_wire(deadline_unix, admit_unix);

    let source = opts.source;
    let frames = opts.frames;
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
    spawn_end_watch(conn.clone(), stop.clone(), quit.clone(), end_reason.clone());

    // Before the handshake resolves the compositor: a handshake that fails still hands back.
    let gamescope_hold = GamescopeHold::new();
    let handshake::Negotiated {
        hello,
        welcome,
        client_label,
        preset: session_preset,
        abr_features,
        client_link,
        probe_only,
        host_link,
        compositor,
        gamescope_route,
        prep,
        joined,
        features,
        resolved,
    } = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        handshake::negotiate(
            &conn,
            &mut send,
            &mut recv,
            &first,
            source,
            frames,
            &bringup,
            quit.clone(),
            stop.clone(),
            initial_grants,
            expires_in_secs,
            resolved,
        ),
    )
    .await
    .map_err(|_| anyhow!("handshake timed out after {HANDSHAKE_TIMEOUT:?}"))??;
    let (ctrl_send, ctrl_recv) = (send, recv);
    if let Some(link) = client_link {
        tracing::info!(
            client_kind = link.kind,
            client_mbps = link.mbps,
            host_kind = host_link.iface_kind,
            host_mbps = host_link.link_mbps,
            "link facts of both ends"
        );
    }
    let join_live = joined.is_some();
    let reframe_to = joined.as_ref().map(|(_, view)| {
        (
            punktfunk_core::video_fit::VideoFit::from_wire(hello.video_fit),
            *view,
        )
    });
    // Filled by the stream thread's encoder open; the input thread reads it.
    let frame_map = input::FrameMap::default();
    // Filled by the stream thread once it has a gamescope; the clipboard reads it.
    let gamescope_xwayland = pf_clipboard::GamescopeXwayland::default();
    // Live reconfigure is off for gamescope (resize must not relaunch the title),
    // `identity: per-client-mode` (resize would resolve a different slot), a monitor
    // mirror (physical head ignores the requested mode) and a `join` session (the mode
    // is the owner's). Synthetic stays on. Captured once here.
    let live_reconfig_ok = {
        let per_client_mode_identity = crate::vdisplay::policy::prefs()
            .configured_effective()
            .is_some_and(|e| e.identity == crate::vdisplay::policy::Identity::PerClientMode);
        // Pin at bring-up; a console change mid-session must not change this session's answer.
        let mirrored = crate::session_plan::mirrored();
        reconfig_allowed(compositor, per_client_mode_identity, mirrored || join_live)
    };
    // `Copy` so the control task's `async move` and SessionContext both keep it.
    let codec = crate::encode::codec_from_wire(welcome.codec);
    tracing::info!(
        %peer,
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
    let SessionWiring {
        control: control_ends,
        stream: stream_ends,
        shard,
        shared,
    } = SessionWiring::new(
        &welcome,
        source,
        crate::send_pacing::Ports::of(
            (host_link.iface_kind, host_link.link_mbps),
            client_link.map_or((0, 0), |l| (l.kind, l.mbps)),
        ),
    );
    // Shard renegotiation only if `Hello::max_shard_payload` and not PyroWave (PyroWave pins
    // the Welcome value for the session; a mid-stream re-key would desync). Not for a browser
    // either: the driver's targets are UDP-over-IP maths, and a WebTransport datagram also
    // carries QUIC and HTTP/3 framing, so a grow it computed would not fit.
    let shard_reneg = (hello.max_shard_payload > 0
        && codec != crate::encode::Codec::PyroWave
        && matches!(data_plane, DataPlane::Shared(_)))
    .then_some(wire_mtu::ShardReneg {
        client_ceiling: hello.max_shard_payload,
        change_tx: shard.change_tx,
        ack_rx: shard.ack_rx,
        apply_tx: shard.apply_tx,
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
    // Only sources that can keep encoder and packetizer FEC in one wire budget adapt it.
    // Synthetic-abr derives frame bytes from FEC every frame; the virtual path publishes
    // a proposal only after its encoder accepts the matching rate. Fixed synthetic and
    // the standalone software source have no coordinated retarget path.
    let adaptive_fec = adaptive_fec_for(source, fec_static_override().is_some());
    // Negotiated rate; PyroWave retarget-refusals ack this pin.
    let session_bitrate_kbps = welcome.bitrate_kbps;
    // Control task flips on `ClipControl`; lifecycle clears it if CLIPBOARD is revoked.
    let clip_enabled = Arc::new(AtomicBool::new(false));
    let clip = start_clipboard(
        &conn,
        initial_grants,
        clip_enabled.clone(),
        clip_target(compositor, &gamescope_xwayland),
    )
    .await;
    let clip_available = clip.available;
    // Lifecycle and the per-session management routes → control task. Both lanes stay
    // open for the whole session: the console can re-point or mute an anonymous client too.
    let (access_tx, access_rx) = tokio::sync::mpsc::unbounded_channel::<AccessUpdate>();
    let (audio_tx, audio_rx) =
        tokio::sync::mpsc::unbounded_channel::<punktfunk_core::quic::AudioState>();
    // Input thread → client: which player each of this session's pads is.
    let (pad_tx, pad_rx) = tokio::sync::mpsc::unbounded_channel::<input::PadToClient>();
    let pad_writes = features.has(punktfunk_core::quic::v2::registry::FEATURE_PAD_WRITES);
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
        pad_slots: pad_slots.clone(),
        fingerprint: session_fp_hex.clone(),
        preset: session_preset.clone(),
        profile: Some(profile_ref.clone()),
        pad_owner: pad_id.owner,
        preferred_pad_slot: Arc::new(std::sync::atomic::AtomicU8::new(
            preferred_pad_slot.unwrap_or(crate::session_status::NO_PAD_SLOT),
        )),
        // Written by the input thread below, read by `GET /session/{id}/pads`.
        pads: Arc::new(crate::pad_feed::PadFeed::new()),
    };
    // One bounded channel for pointer/keyboard and rich input, fed by the datagram loop and
    // by the control loop's key edges. Unbounded is RSS DoS: the producer outruns the
    // consumer; pen batches amplify. Drop is correct — stale input is already worthless;
    // the injector re-syncs from the next event.
    const INPUT_QUEUE_DEPTH: usize = 1024;
    let (input_tx, input_rx) = std::sync::mpsc::sync_channel::<ClientInput>(INPUT_QUEUE_DEPTH);
    // The client's feedback datagrams, datagram reader → control task. Unbounded: a client sends
    // a few per report window.
    let (feedback_tx, feedback_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(control::run(control::Task {
        ctrl_send,
        ctrl_recv,
        clock: conn.v2_session().clock.clone(),
        input_tx: input_tx.clone(),
        initial_mode: hello.mode,
        stream_config: v2msg::StreamConfig {
            epoch: 0,
            mode: welcome.mode,
            codec: welcome.codec,
            bit_depth: welcome.bit_depth,
            color: [
                welcome.color.primaries,
                welcome.color.transfer,
                welcome.color.matrix,
                welcome.color.full_range,
            ],
            chroma_format: welcome.chroma_format,
            host_iface_kind: host_link.iface_kind,
            host_link_mbps: host_link.link_mbps,
            host_sndbuf_kb: host_link.sndbuf_kb,
            host_forced_shape: host_link.forced_shape,
        },
        anchored: features.has(punktfunk_core::quic::v2::registry::FEATURE_STREAM_CONFIG),
        codec,
        live_reconfig_ok,
        adaptive_fec,
        session_bitrate_kbps,
        // Automatic, and negotiable: PyroWave resolves a pin the client cannot
        // move either, so the governor must not move it for it.
        bitrate_automatic: hello.bitrate_kbps == 0 && codec != crate::encode::Codec::PyroWave,
        // Automatic PyroWave: the client's bring-up ramp may lower the pin
        // once, while the ramp window is still open.
        pyrowave_automatic: hello.bitrate_kbps == 0 && codec == crate::encode::Codec::PyroWave,
        wire_bytes: u64::from(welcome.shard_payload)
            + punktfunk_core::abr::budget::SHARD_WIRE_OVERHEAD,
        audio_kbps: audio_reserved_kbps(&welcome),
        ack_reason: abr_features & punktfunk_core::quic::EXT_ABR_ACK_REASON != 0,
        feedback_rx,
        probe_only,
        ends: control_ends,
        shared: shared.clone(),
        clip_enabled: clip_enabled.clone(),
        clip,
        session_grants: session_grants.clone(),
        access_rx,
        audio_rx,
        pad_rx,
        launch_outcome_rx,
        peer: peer.ip(),
        plane: conn.plane(),
        counters: counters.clone(),
        stats: stats.clone(),
    }));
    let client_name = client_name(np, session_fp_hex.as_deref(), &hello);
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

    let planes = SessionPlanes::mint(
        joined.as_ref().map(|(d, _)| d),
        compositor,
        gamescope_route.as_ref(),
        &resolved,
        &inj_tx,
        mic_tx,
    );
    let input_route = planes.input_route.clone();

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
        let seat_dev = planes.seat_dev.clone();
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
                        Some(pad_tx),
                        pad_writes,
                        grants,
                        frame_map,
                        pad_feed,
                        seat_dev,
                        stop,
                        counters,
                    )
                }
            })
            .context("spawn input thread")?
    };
    input::spawn_datagram_reader(
        conn.clone(),
        session_grants.clone(),
        counters.clone(),
        planes.mic_tx.clone(),
        input_tx,
        feedback_tx,
    );

    // Handshake complete: CONNECTED. A client rejected earlier never emits either.
    emit_connected(
        &conn,
        crate::events::ClientRef {
            name: client_name.clone().unwrap_or_default(),
            fingerprint: session_fp_hex.clone(),
            plane: conn.plane(),
            preset: session_preset.clone(),
            profile: Some(profile_ref.clone()),
        },
    );

    // Mode-conflict admission: later clients see this identity + mode + stop (and may `steal`).
    // The audio thread publishes the sink it captures here, for a joiner to tap.
    let audio_sink: Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let _live_guard = {
        let id = conn.peer_fingerprint();
        let label = id
            .map(|fp| hex::encode(&fp[..4]))
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
                isolation: planes.isolation.clone(),
                audio_sink: audio_sink.clone(),
            },
            Some(conn.v2_session().session_id),
        )
    };

    // `CLIENT_CAP_KEEP_HOST_AUDIO`: taken before the audio thread spawns, which opens
    // capture straight away and reads this to pick its topology. RAII.
    let _keep_host_audio = (hello.client_caps & punktfunk_core::quic::CLIENT_CAP_KEEP_HOST_AUDIO
        != 0)
        .then(crate::audio::capture_policy::keep_host_audio_guard);

    // Not for the two frame-arithmetic sources: their clients want nothing else on the wire,
    // and the rig's budget carries the audio reservation without a capture behind it.
    // Best-effort: a spawn error must not early-return (threads already up).
    let audio_handle = if !matches!(
        opts.source,
        Punktfunk1Source::Synthetic | Punktfunk1Source::SyntheticAbr(..)
    ) {
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
        let iso_sink = planes.isolation.clone().and_then(|i| i.sink);
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

    if welcome.color.is_hdr() {
        send_hdr_baseline(&conn, hello.display_hdr);
    }
    if opts.source == Punktfunk1Source::Synthetic
        && std::env::var("PUNKTFUNK_TEST_FEEDBACK").as_deref() == Ok("1")
    {
        send_test_feedback(&conn);
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
        plane: conn.plane(),
        preset: session_preset.clone(),
        profile: Some(profile_ref.clone()),
    });
    // Linux `PUNKTFUNK_PIN_CLOCKS`: refcounted vendor clock floor while any session streams.
    #[cfg(target_os = "linux")]
    let _clock_pin = crate::gpuclocks::session_pin();
    let launch_target = match resolve_launch(hello.launch.as_deref(), &launch_outcome_tx).await? {
        Some(t) => Some(t),
        None => home_launch(hello.launch.as_deref(), &resolved),
    };
    #[cfg(target_os = "windows")]
    let launch_for_dp = launch_target.as_ref().and(hello.launch.clone());
    #[cfg(not(target_os = "windows"))]
    let launch_for_dp = launch_target.as_ref().and_then(|t| t.command.clone());
    // Stats label: device-fingerprint prefix, else peer IP (anonymous, `--open`).
    let client_label = conn
        .peer_fingerprint()
        .map(|fp| hex::encode(fp)[..12].to_string())
        .unwrap_or_else(|| conn.remote_address().ip().to_string());
    let launch_owner = crate::session_launch::LaunchOwner {
        client: client_label.clone(),
        fingerprint: conn.peer_fingerprint().map(hex::encode),
        plane: conn.plane(),
        preset: session_preset.clone(),
        profile: Some(profile_ref.clone()),
        pad: welcome.gamepad,
        pad_slots: Some(controls.pad_slots.clone()),
    };
    let (prep_cmds, prep_env) = launch_prep(&hello, &welcome, session_preset.as_ref());
    // Reprieve, claim, prep and the launch hold, before the display opens. `block_in_place`:
    // operator code is blocking and this is a multi-thread runtime.
    let crate::session_launch::Prepared {
        claim: launch_claim,
        stamp: launch_stamp,
        prep: _prep,
        declined,
        waiting: _waiting,
    } = tokio::task::block_in_place(|| {
        crate::session_launch::prepare(
            launch_target.as_ref(),
            &launch_owner,
            &prep_cmds,
            &prep_env,
            &|| stop.load(Ordering::Relaxed),
        )
    });
    // The title's files never arrived: stream without it, and say why.
    let (launch_target, launch_for_dp) = match declined {
        Some(sentence) => {
            let _ = launch_outcome_tx.send(punktfunk_core::quic::LaunchOutcome::new(
                punktfunk_core::quic::LaunchOutcomeKind::Refused,
                &sentence,
            ));
            (None, None)
        }
        None => (launch_target, launch_for_dp),
    };
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
    let shared_dp = shared;
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
    let isolation_dp = planes.isolation.clone();
    #[cfg(target_os = "linux")]
    let input_route_dp = input_route.clone();
    #[cfg(target_os = "linux")]
    let inj_shared_tx_dp = inj_tx.clone();
    #[cfg(target_os = "linux")]
    let inj_session_tx_dp = planes.inj_session_tx.clone();
    // Client address: what the registry groups sessions of one NAT or tunnel by.
    let peer_ip = conn.remote_address().ip();
    let plane = conn.plane();
    let result: Result<()> = async {
        let stream_thread = tokio::task::spawn_blocking(move || -> Result<()> {
            let (transport, wire_sock, media) = bind_data_plane(data_plane, &bringup_dp)?;
            let session = Session::new(cfg, media, transport)
                .map_err(|e| anyhow!("host session: {e:?}"))?;
            let mut common = StreamCommon {
                session,
                mode,
                seconds,
                stop: stop_stream,
                quit: quit_stream,
                end_reason: end_reason_stream,
                counters: counters_stream,
                ends: stream_ends,
                shared: shared_dp,
                bitrate_kbps,
                audio_reserved_kbps,
                shard_payload: welcome.shard_payload,
                timing_conn,
                probe_seq,
                stats: stats_dp,
                client_label,
                bringup: bringup_dp,
                wire_sock,
                codec,
                controls,
                client_name,
                hdr,
                bit_depth,
                chroma,
            };
            // A display prep that started at Welcome (Windows) goes unreceived here and
            // aborts into keep-alive: the tag arrives on Start, after the prep began.
            if probe_only {
                return stream::probe_only_stream(&mut common, probe_seq);
            }
            match source {
                Punktfunk1Source::Software => software_stream(
                    &mut common.session,
                    codec,
                    mode,
                    bitrate_kbps,
                    &common.stop,
                    &common.ends.probe_rx,
                    &common.ends.probe_result_tx,
                    &common.shared.fec_target,
                    probe_seq,
                ),
                Punktfunk1Source::Synthetic => synthetic_stream(
                    &mut common.session,
                    frames,
                    &common.stop,
                    &common.ends,
                    &common.shared.fec_target,
                    common.timing_conn.as_ref(),
                    probe_seq,
                ),
                Punktfunk1Source::SyntheticAbr(shape) => synthetic_abr_stream(SynthAbrContext {
                    common,
                    content: shape.content,
                    recovery: shape.recovery,
                    answer: shape.answer,
                    idr_pct: shape.idr_pct,
                    bringup_delay: shape.bringup,
                    fit_pin: hello.bitrate_kbps == 0 && codec == crate::encode::Codec::PyroWave,
                    plane,
                    peer: peer_ip,
                }),
                Punktfunk1Source::Virtual => {
                    let compositor = compositor
                        .expect("the Virtual source resolves a compositor during the handshake");
                    let ctx = SessionContext {
                        common,
                        compositor,
                        gamescope_route,
                        bitrate_auto,
                        cursor_forward,
                        streamed_au,
                        multi_slice,
                        conn: conn_stream,
                        launch: launch_for_dp,
                        launch_target,
                        launch_claim,
                        launch_stamp,
                        launch_owner,
                        launch_outcome: launch_outcome_dp,
                        client_hdr,
                        join_live,
                        reframe_to,
                        frame_map,
                        #[cfg(target_os = "linux")]
                        gamescope_xwayland,
                        resize_ms: resize_ms_dp,
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

    teardown(&stop, &conn, &result, audio_handle, input_handle).await;
    // After teardown: the last hold out hands the TV's gaming session back.
    drop(gamescope_hold);
    result.map(|()| Served::Session)
}

/// What admission resolved for this device: its effective grant mask, deadline and the record's
/// watch. Anonymous (`--open`) and an identity with no record keep full control — nothing on the
/// trust record to enforce.
struct Admission {
    grants: u32,
    deadline_unix: Option<i64>,
    watch: Option<tokio::sync::watch::Receiver<crate::native_pairing::AccessState>>,
    /// When admission ran; Welcome's remaining time counts from here.
    at_unix: i64,
    /// Counts this session while it runs. Held to the end of the session, so an error return or
    /// a cancelled task releases it too — a record granted "this session" is dropped once the
    /// guard falls and nothing reconnects inside the grace.
    session: Option<crate::native_pairing::SessionCountGuard>,
}

/// Grants once at admission, with its two typed refusals: access that expired since the pairing
/// gate, and a library launch without `GRANT_LAUNCH` — refused before the handshake, not a silent
/// bare desktop.
async fn admit(
    host: &SessionHost,
    fp_hex: Option<&str>,
    conn: &link::SessionLink,
    first: &ClientHello,
) -> Result<Admission> {
    let at_unix = crate::clock::unix_secs();
    let (grants, deadline_unix, watch) = match fp_hex {
        Some(fp_hex) => match host.np.effective(fp_hex, at_unix) {
            Some(mask) => {
                // Subscribe before reading the deadline so a racing edit lands in this borrow
                // or as the first change — never in a gap.
                let rx = host.np.subscribe(fp_hex);
                let deadline = rx.borrow().deadline_unix;
                (mask, deadline, Some(rx))
            }
            // Expired between the pairing gate and here: typed expiry, not a setup error.
            None if host.opts.require_pairing => {
                close_rejected(conn, punktfunk_core::reject::RejectReason::AccessExpired).await;
                anyhow::bail!("access expired between admission and session setup");
            }
            // `--open`: unpaired / expired identities keep full control.
            None => (GRANT_ALL, None, None),
        },
        None => (GRANT_ALL, None, None),
    };
    let session = fp_hex.map(|fp_hex| host.np.session_started(fp_hex));
    if grants & GRANT_LAUNCH == 0 && first.hello.launch.is_some() {
        close_rejected(
            conn,
            punktfunk_core::reject::RejectReason::LaunchNotPermitted,
        )
        .await;
        anyhow::bail!("client requested a library launch without the LAUNCH grant");
    }
    Ok(Admission {
        grants,
        deadline_unix,
        watch,
        at_unix,
        session,
    })
}

/// Stop the session when its connection closes, and latch why for the summary. First writer
/// wins, so the host's own close (game exit, clean finish) beats the `Lost` this would read it
/// back as.
fn spawn_end_watch(
    conn: link::SessionLink,
    stop: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    end_reason: Arc<AtomicU8>,
) {
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

/// The clipboard coordinator. Without CLIPBOARD it never starts (a watcher that doesn't exist
/// can't leak): an inert handle (`available: false`) keeps the control task's arms uniform —
/// NOT_PERMITTED, and the decline loop still answers stray fetches. Fetch transfers are quinn
/// streams, so a browser takes the inert arm until the control plane is carrier-agnostic.
async fn start_clipboard(
    conn: &link::SessionLink,
    grants: u32,
    enabled: Arc<AtomicBool>,
    target: pf_clipboard::ClipTarget,
) -> pf_clipboard::ClipCoord {
    let quic = (grants & GRANT_CLIPBOARD != 0)
        .then(|| conn.as_quic().cloned())
        .flatten();
    if let Some(quic) = quic {
        return pf_clipboard::start(quic, enabled, target).await;
    }
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_offer_tx, offer_rx) = tokio::sync::mpsc::unbounded_channel();
    pf_clipboard::ClipCoord {
        available: false,
        cmd_tx,
        offer_rx,
    }
}

/// The clipboard a session on `compositor` shares. gamescope keeps its own, on its Xwayland;
/// the desktop the process env names is somebody else's.
fn clip_target(
    compositor: Option<crate::vdisplay::Compositor>,
    gamescope_xwayland: &pf_clipboard::GamescopeXwayland,
) -> pf_clipboard::ClipTarget {
    match compositor {
        None => pf_clipboard::ClipTarget::None,
        Some(crate::vdisplay::Compositor::Gamescope) => {
            pf_clipboard::ClipTarget::Gamescope(gamescope_xwayland.clone())
        }
        Some(_) => pf_clipboard::ClipTarget::Session,
    }
}

/// Trust-store name (a console rename wins), else the sanitized Hello name. `None` if nameless.
/// Events, hook filters, the stream marker and the tray all show this one.
fn client_name(np: &NativePairing, fp_hex: Option<&str>, hello: &Hello) -> Option<String> {
    fp_hex
        .and_then(|fp| np.list().into_iter().find(|c| c.fingerprint == fp))
        .map(|c| c.name)
        .or_else(|| {
            let raw = hello.name.as_deref().unwrap_or("").trim();
            (!raw.is_empty())
                .then(|| crate::native_pairing::sanitize_device_name(raw, fp_hex.unwrap_or("")))
        })
}

/// This session's input and mic planes. An isolated gamescope session
/// (`compositor_route::session_is_isolated`) gets its own; everyone else shares the
/// host-lifetime ones. Dropping it closes the pinned EIS connection and the isolated mic.
struct SessionPlanes {
    /// Linux only. Identity is the device-fingerprint prefix, so keep-alive hands a kept spawn
    /// back to the same client.
    isolation: Option<crate::vdisplay::SessionIsolation>,
    /// Where this session's virtual pads are exposed, so its seat's Steam opens those and no
    /// other seat's. `None` on every host without the filter, which is today's box-wide pads.
    seat_dev: Option<std::path::PathBuf>,
    /// Pointer and keyboard: the pinned injector, else the host-lifetime one. Swappable.
    input_route: input::InputRoute,
    #[cfg(target_os = "linux")]
    inj_session_tx: Option<std::sync::mpsc::Sender<InputEvent>>,
    /// The 0xCB uplink: this session's `punktfunk-mic-{id}` pump, else the shared one.
    mic_tx: std::sync::mpsc::SyncSender<crate::audio::MicFrame>,
    #[cfg(target_os = "linux")]
    _mic: Option<crate::audio::MicPump>,
    /// The shared mic is the box's default source while this session lives. An isolated
    /// session's own mic is pinned by `PULSE_SOURCE` and claims nothing.
    #[cfg(target_os = "linux")]
    _mic_default: Option<crate::audio::DefaultMicClaim>,
    #[cfg(target_os = "linux")]
    _injector: Option<crate::inject::InjectorService>,
    /// This session's hold on its isolation id.
    #[cfg(target_os = "linux")]
    _seat: Option<SeatClaim>,
}

impl SessionPlanes {
    /// Minted after the handshake, before the input and audio threads.
    fn mint(
        joined: Option<&crate::vdisplay::admission::LiveDisplay>,
        compositor: Option<crate::vdisplay::Compositor>,
        route: Option<&crate::vdisplay::GamescopeRoute>,
        profile: &crate::profiles::Resolved,
        inj_tx: &std::sync::mpsc::Sender<InputEvent>,
        mic_tx: std::sync::mpsc::SyncSender<crate::audio::MicFrame>,
    ) -> SessionPlanes {
        #[cfg(target_os = "linux")]
        {
            let mut seat = None;
            let isolation = match joined {
                // A joiner uses the owner's planes: its input relay and sink. A second mic source
                // of the same name would split the owner's, so its mic stays on the shared one.
                Some(d) => d
                    .isolation
                    .clone()
                    .map(|i| crate::vdisplay::SessionIsolation {
                        mic_source: None,
                        ..i
                    }),
                None => compositor
                    .filter(|c| crate::compositor_route::session_is_isolated(*c, route))
                    .map(|_| {
                        // The profile is the seat: its first session takes the seat's id and
                        // home, a concurrent second one `<seat>-2` and the box's Steam.
                        let base = seat_id(&profile.id);
                        let claim = SeatClaim::take(&base);
                        let is_seat =
                            matches!(profile.os_account, crate::profiles::OsAccount::Seat { .. });
                        let home =
                            (is_seat && claim.is_first(&base)).then_some(profile.id.as_str());
                        let iso = session_isolation(&claim.0, home);
                        tracing::info!(id = %claim.0, profile = %profile.id,
                            sink = iso.sink.as_deref().unwrap_or("-"),
                            "isolated gamescope session — per-session input/audio/mic planes");
                        seat = Some(claim);
                        iso
                    }),
            };
            let seat_dev = isolation
                .as_ref()
                .and_then(crate::vdisplay::seat_device_dir);
            let injector = isolation
                .as_ref()
                .map(|i| crate::inject::InjectorService::start_at(i.ei_relay.clone()));
            let inj_session_tx = injector.as_ref().map(|s| s.sender());
            let input_route =
                input::InputRoute::new(inj_session_tx.clone().unwrap_or_else(|| inj_tx.clone()));
            let mic = isolation
                .as_ref()
                .and_then(|i| i.mic_source.clone())
                .map(|name| crate::audio::MicPump::start_named(Some(name)));
            let mic_tx = mic.as_ref().map(|p| p.sender()).unwrap_or(mic_tx);
            let mic_default = mic.is_none().then(crate::audio::claim_default_mic);
            SessionPlanes {
                isolation,
                seat_dev,
                input_route,
                inj_session_tx,
                mic_tx,
                _mic: mic,
                _mic_default: mic_default,
                _injector: injector,
                _seat: seat,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (joined, compositor, route, profile);
            SessionPlanes {
                isolation: None,
                seat_dev: None,
                input_route: input::InputRoute::new(inj_tx.clone()),
                mic_tx,
            }
        }
    }
}

/// `ClientConnected` now, and `ClientDisconnected` with its reason once the connection closes.
fn emit_connected(conn: &link::SessionLink, client: crate::events::ClientRef) {
    crate::events::emit(crate::events::EventKind::ClientConnected {
        client: client.clone(),
    });
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
            client,
            reason: why,
        });
    });
}

/// HDR10 baseline at start, from the client's display volume (`Hello::display_hdr`, which
/// its EDID advertises), else generic HDR10. The virtual stream then sends the source's real
/// mastering on capture start and keyframes; this covers synthetic and the pre-capture gap.
fn send_hdr_baseline(conn: &link::SessionLink, display_hdr: Option<punktfunk_core::quic::HdrMeta>) {
    let meta = crate::encode::hdr_meta_to_wire(display_hdr.map_or_else(
        pf_frame::hdr::generic_hdr10,
        crate::encode::hdr_meta_from_wire,
    ));
    let _ = conn.send_datagram(punktfunk_core::quic::encode_hdr_meta_datagram(&meta));
    tracing::info!(
        client_volume = display_hdr.is_some(),
        "sent HDR10 static metadata (0xCE baseline)"
    );
}

/// `PUNKTFUNK_TEST_FEEDBACK=1` on the synthetic source: rumble (0xCA) and HID output (0xCD)
/// for a loopback client, with no real pad.
fn send_test_feedback(conn: &link::SessionLink) {
    use punktfunk_core::quic::HidOutput;
    // 400 ms TTL + both trigger motors. Trigger levels differ from each other and the
    // handles so a wrong-offset decoder cannot hide behind a plausible zero.
    let d =
        punktfunk_core::quic::encode_rumble_datagram_v3(0, 0x4000, 0x8000, 0, 400, 0x2000, 0x6000);
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

/// One library lookup: command, title, process identity. The client picks an existing id,
/// never a command. An id this host no longer has is refused onto `outcome`. Blocking: plugin
/// entries ask over loopback from this async context.
async fn resolve_launch(
    launch: Option<&str>,
    outcome: &crate::gamelease::OutcomeTx,
) -> Result<Option<crate::library::LaunchTarget>> {
    let Some(id) = launch else {
        return Ok(None);
    };
    let owned = id.to_string();
    let found = tokio::task::spawn_blocking(move || crate::library::resolve_launch(&owned))
        .await
        .context("resolve the session's library launch")?;
    match &found {
        Some(t) => tracing::info!(
            launch_id = id,
            title = %t.game.title,
            command = t.command.as_deref().unwrap_or("-"),
            "resolved library launch for this session"
        ),
        None => {
            tracing::warn!(
                launch_id = id,
                "client requested a launch id not in this host's library — ignoring"
            );
            let _ = outcome.send(punktfunk_core::quic::LaunchOutcome::new(
                punktfunk_core::quic::LaunchOutcomeKind::Refused,
                "Couldn't start that title — this host doesn't have it in its library \
                 any more.",
            ));
        }
    }
    Ok(found)
}

/// What a bare connect opens: Big Picture for a seat profile whose home is `bigpicture`. A
/// connect that named a title, or the owner's, opens nothing more.
fn home_launch(
    asked: Option<&str>,
    profile: &crate::profiles::Resolved,
) -> Option<crate::library::LaunchTarget> {
    if asked.is_some() || !opens_big_picture(profile) {
        return None;
    }
    #[cfg(not(windows))]
    return crate::library::big_picture_launch();
    #[cfg(windows)]
    None
}

/// A seat profile whose bare connect opens Big Picture in its own gamescope.
pub(crate) fn opens_big_picture(profile: &crate::profiles::Resolved) -> bool {
    matches!(profile.os_account, crate::profiles::OsAccount::Seat { .. })
        && profile.home == crate::profiles::Home::Bigpicture
}

/// The launched title's prep steps and their environment: `PF_APP_ID` and `PF_STREAM_*`, so a
/// step can set a per-mode FPS cap, and `PF_PRESET_*`, so it can tell a docked session from a
/// handheld one. Empty without a launch.
fn launch_prep(
    hello: &Hello,
    welcome: &Welcome,
    preset: Option<&crate::events::PresetRef>,
) -> (Vec<crate::hooks::PrepCmd>, Vec<(String, String)>) {
    let Some(id) = hello.launch.as_deref() else {
        return (Vec::new(), Vec::new());
    };
    let mut env = vec![("PF_APP_ID".to_string(), id.to_string())];
    if let Some(p) = preset {
        env.push(("PF_PRESET_ID".to_string(), p.id.clone()));
        env.push(("PF_PRESET_NAME".to_string(), p.name.clone()));
    }
    env.extend(crate::hooks::prep_mode_env(
        hello.mode.width,
        hello.mode.height,
        hello.mode.refresh_hz,
        welcome.color.is_hdr(),
    ));
    (crate::library::prep_for(id), env)
}

type BoundPlane = (
    Box<dyn punktfunk_core::transport::Transport>,
    Option<std::net::UdpSocket>,
    punktfunk_core::session::MediaV2,
);

/// The video transport, the media socket's clone for the `wire egress` probe, and the media
/// framing. A browser's video rides its WebTransport datagrams, unsealed inside that
/// encryption; a native session's leaves from the endpoint's socket toward the connection's
/// validated address, keyed from its exporter.
fn bind_data_plane(data_plane: DataPlane, bringup: &crate::bringup::Trace) -> Result<BoundPlane> {
    bringup.mark("punch_done");
    match data_plane {
        DataPlane::Web(plane) => {
            let v2 = plane.v2().clone();
            let media = punktfunk_core::session::MediaV2 {
                clock_origin_ns: v2.clock.origin_ns(),
                keys: None,
                clock: Some(v2.clock.clone()),
            };
            Ok((Box::new(plane), None, media))
        }
        DataPlane::Shared(v2) => {
            let suite = v2
                .suite()
                .ok_or_else(|| anyhow!("punktfunk/2 session reached media with no suite"))?;
            let keys = endpoint::media_keys(&v2.conn, &v2.session_id, suite)
                .ok_or_else(|| anyhow!("punktfunk/2 media keys: exporter refused"))?;
            let sender = punktfunk_core::transport::shared::MediaSender::new(
                &v2.media_socket,
                v2.conn.clone(),
            )
            .context("punktfunk/2 media sender")?;
            let media = punktfunk_core::session::MediaV2 {
                clock_origin_ns: v2.clock.origin_ns(),
                keys: Some(keys),
                clock: Some(v2.clock.clone()),
            };
            Ok((Box::new(sender), v2.media_socket.try_clone().ok(), media))
        }
    }
}

/// Every exit: stop audio, close the connection, join the side threads. The close ends the
/// datagram task and with it the input thread. The join is bounded: a stuck side thread must not
/// hold the permit or the admission entry.
async fn teardown(
    stop: &AtomicBool,
    conn: &link::SessionLink,
    result: &Result<()>,
    audio_handle: Option<std::thread::JoinHandle<()>>,
    input_handle: std::thread::JoinHandle<()>,
) {
    stop.store(true, Ordering::SeqCst);
    conn.close(
        if result.is_ok() { 0u32 } else { 1u32 },
        if result.is_ok() { b"done" } else { b"error" },
    );
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

/// A seat profile's Steam home while **Steam per seat** is on, or `None` for the box's own.
#[cfg(target_os = "linux")]
fn seat_home_for(profile: Option<&str>, on: bool) -> Option<std::path::PathBuf> {
    profile.filter(|_| on).map(pf_paths::seat_home)
}

/// The seat a profile streams on: the head of its id. Short enough for a socket name. One
/// function, because the pre-warm has to name the same seat a connect does or the registry
/// hands its parked display to nobody.
#[cfg(target_os = "linux")]
fn seat_id(profile_id: &str) -> String {
    profile_id[..profile_id.len().min(8)].to_string()
}

/// Isolation ids of the sessions streaming now. A second session on one profile is
/// `<seat>-2`: planes of its own, and no claim on the seat's home or its parked display.
#[cfg(target_os = "linux")]
static LIVE_SEATS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// An isolation id held for one session; dropping it frees the id.
#[cfg(target_os = "linux")]
struct SeatClaim(String);

#[cfg(target_os = "linux")]
impl SeatClaim {
    /// `base`, or the first `base-<n>` no live session holds.
    fn take(base: &str) -> SeatClaim {
        let mut live = LIVE_SEATS.lock().unwrap_or_else(|e| e.into_inner());
        let id = std::iter::once(base.to_string())
            .chain((2..).map(|n| format!("{base}-{n}")))
            .find(|id| !live.contains(id))
            .unwrap_or_else(|| base.to_string());
        live.push(id.clone());
        SeatClaim(id)
    }

    fn is_first(&self, base: &str) -> bool {
        self.0 == base
    }
}

#[cfg(target_os = "linux")]
impl Drop for SeatClaim {
    fn drop(&mut self) {
        let mut live = LIVE_SEATS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = live.iter().position(|id| id == &self.0) {
            live.swap_remove(at);
        }
    }
}

/// The isolated planes `id` streams on. `home` is the seat profile whose Steam home it runs
/// under; `None` keeps the box's own.
///
/// The registry's reuse key is `id` plus that home, so [`prewarm`] builds this value for a seat
/// before its client connects and the connect lands on the display already standing.
#[cfg(target_os = "linux")]
fn session_isolation(id: &str, home: Option<&str>) -> crate::vdisplay::SessionIsolation {
    // Monitor-mode has no per-session sink — audio stays shared; input/mic still isolate.
    let sink =
        crate::audio::per_session_sink_possible().then(|| format!("punktfunk-speaker-iso-{id}"));
    let steam_home = seat_home_for(home, pf_host_config::config().steam_seat_home);
    crate::vdisplay::SessionIsolation::new(
        id.to_string(),
        sink,
        Some(format!("punktfunk-mic-{id}")),
        steam_home,
    )
}

#[cfg(test)]
mod tests;
