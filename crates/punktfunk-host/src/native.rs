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
//! the counterpart. Evidence: `design/` and the tests below.

use anyhow::{anyhow, Context, Result};
// The wire budget, adaptive FEC and their band: one arithmetic, shared with
// the client's controller and the link simulator.
use punktfunk_core::abr::budget::{
    budget_kbps_for_encoder, encoder_kbps_for_budget, FEC_ADAPTIVE_START, MIN_BITRATE_KBPS,
};
use punktfunk_core::config::{CompositorPref, FecConfig, FecScheme, GamepadPref, Role};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::packet::{FLAG_PIC, FLAG_PROBE, FLAG_SOF};
use punktfunk_core::quic::v2::hello::{ClientHello, Ready, ServerHello};
use punktfunk_core::quic::v2::msg as v2msg;
use punktfunk_core::quic::{
    classify, endpoint, pkf1, AccessUpdate, AckReason, BitrateChanged, ClockEcho, ClockProbe,
    ColorInfo, GrantClass, Hello, LinkReport, LossReport, PairRequest, PipelineGap, ProbeResult,
    ProbeShaped, Reconfigure, Reconfigured, RequestKeyframe, RfiRequest, SetBitrate, Welcome,
    GRANT_ALL, GRANT_CLIPBOARD, GRANT_LAUNCH,
};
use punktfunk_core::Session;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

/// Shared with GameStream.
pub(crate) use pf_frame::thread_qos::boost_thread_priority;

// The session's control connection, whichever transport carries it (quinn or WebTransport).
pub(crate) mod link;
/// A seat's Steam, up before its client asks (`design/steam-seats-warm-launch-implementation-plan.md`).
#[cfg(target_os = "linux")]
pub(crate) mod prewarm;
use crate::compositor_route::resolve_compositor;

/// GameStream presents the same virtual pad and must pick `windows_xbox_hid` from this definition.
pub(crate) mod gamepad;
use gamepad::{resolve_gamepad, resolve_pad_kind, route_decision};

mod pairing;
pub(crate) use pairing::{pair_ceremony, PairWire};

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

/// `u32 LE index` then `data[i] = idx + i` (wrapping) — the client byte-checks this.
pub fn test_frame(idx: u32, len: usize) -> Vec<u8> {
    let mut d = vec![0u8; len];
    d[0..4].copy_from_slice(&idx.to_le_bytes());
    for (i, b) in d.iter_mut().enumerate().skip(4) {
        *b = (idx as u8).wrapping_add(i as u8);
    }
    d
}

use punktfunk_core::quic::wall_clock_ns as now_ns;

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

    // A sinkless capturer handed session to session (`AudioCapSlot`, `park_audio_capture`).
    let audio_cap: AudioCapSlot = Arc::new(std::sync::Mutex::new(None));
    // Host-lifetime injector: one RemoteDesktop-portal grant. A CreateSession per session
    // races portal teardown on reconnect and wedges KWin EIS. Gamepads stay per-session.
    let injector = crate::inject::InjectorService::start();
    // A crashed host's claims left the box's audio defaults on its own nodes. Off-thread: a
    // sick PipeWire must not hold up serving; a session's claim waits on the same lock.
    std::thread::spawn(crate::audio::heal_audio_defaults);
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
    // Once the host serves: a seat's Steam takes half a minute to boot, and the point is that it
    // has already booted when its device connects.
    #[cfg(target_os = "linux")]
    prewarm::spawn_run("host start");

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
        let last_pairing = last_pairing.clone();
        let stats = stats.clone();
        let inj_tx = injector.sender();
        let mic_tx = mic_service.sender();
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
            prewarm::spawn_run("session end");
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
async fn close_rejected(conn: &link::SessionLink, reason: punktfunk_core::reject::RejectReason) {
    conn.refuse(reason.close_code(), &reason.to_string()).await;
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
    let mut warned = spent_warnings(deadline, crate::clock::unix_secs());
    // `power.*` ending every session: typed close so the client does not see a transport error.
    let mut power_rx = crate::power::closing_rx();
    loop {
        let now = crate::clock::unix_secs();
        if let Some(d) = deadline {
            if now >= d {
                // Wall clock at fire: `d − now` is recomputed each lap, so an NTP step moves it.
                tracing::info!(
                    device = %device.name,
                    fingerprint = %device.fingerprint,
                    "temporary access expired — closing this device's session"
                );
                crate::events::emit(crate::events::EventKind::AccessExpired { device });
                close_rejected(&conn, punktfunk_core::reject::RejectReason::AccessExpired).await;
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
            () = tokio::time::sleep(access_sleep(deadline, &warned, crate::clock::unix_secs())) => {}
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
                    close_rejected(&conn, punktfunk_core::reject::RejectReason::AccessExpired).await;
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
                let now = crate::clock::unix_secs();
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
                    close_rejected(&conn, punktfunk_core::reject::RejectReason::HostPower).await;
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

/// PyroWave pins the host's bits per pixel (row `pyrowave_bpp`) for the negotiated mode, not
/// the 20 Mbps H.26x default. ABR stays off; mid-stream retargets are refused. A client rate
/// is ignored: bits per pixel is the quality knob, and it holds across modes. Every pin goes
/// through `PUNKTFUNK_PYROWAVE_MAX_MBPS`. H.26x/AV1 explicit rates stand.
fn resolve_bitrate_kbps_for(
    codec: crate::encode::Codec,
    requested: u32,
    mode: &punktfunk_core::config::Mode,
    chroma: crate::encode::ChromaFormat,
    bit_depth: u8,
) -> u32 {
    resolve_bitrate_kbps_under(
        codec,
        requested,
        mode,
        chroma,
        bit_depth,
        pyrowave_auto_pin_ceiling_kbps,
    )
}

/// [`resolve_bitrate_kbps_for`] with the PyroWave ceiling (kbps) read through `ceiling`, so a
/// test hands one in without writing the process environment.
fn resolve_bitrate_kbps_under(
    codec: crate::encode::Codec,
    requested: u32,
    mode: &punktfunk_core::config::Mode,
    chroma: crate::encode::ChromaFormat,
    bit_depth: u8,
    ceiling: fn() -> Option<u32>,
) -> u32 {
    if codec == crate::encode::Codec::PyroWave {
        if requested != 0 {
            tracing::warn!(
                requested_kbps = requested,
                "a client bitrate does not apply to PyroWave — using the host's bits per pixel"
            );
        }
        let bpp = pf_host_config::config().pyrowave_bpp;
        let pin = pyrowave_pin_kbps(mode, chroma, bit_depth, bpp);
        // Open-loop pin can outrun the link. `PUNKTFUNK_PYROWAVE_MAX_MBPS` caps it;
        // unset ⇒ no cap.
        if let Some(ceiling) = ceiling() {
            if pin > ceiling {
                tracing::warn!(
                    pin_kbps = pin,
                    ceiling_kbps = ceiling,
                    "PyroWave bitrate pin exceeds PUNKTFUNK_PYROWAVE_MAX_MBPS — capping to it"
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

/// `bpp` bits per pixel for a 4:2:0 SDR frame. 4:4:4 carries twice the samples but costs
/// ×1.625, since chroma compresses better than luma; 10-bit planes add 15 %.
fn pyrowave_pin_kbps(
    mode: &punktfunk_core::config::Mode,
    chroma: crate::encode::ChromaFormat,
    bit_depth: u8,
    bpp: f64,
) -> u32 {
    let mut bpp = bpp;
    if chroma.is_444() {
        bpp *= 1.625;
    }
    if bit_depth >= 10 {
        bpp *= 1.15;
    }
    let px_per_s =
        f64::from(mode.width) * f64::from(mode.height) * f64::from(mode.refresh_hz.max(1));
    // `as` saturates, so a huge mode lands on the clamp.
    ((px_per_s * bpp / 1000.0) as u32).clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS)
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

/// Whether this source adapts FEC: only sources that can keep encoder and packetizer
/// FEC in one wire budget. Synthetic-abr derives frame bytes from FEC every frame;
/// the virtual path publishes a proposal only after its encoder accepts the matching
/// rate. Fixed synthetic and the standalone software source have no retarget path.
fn adaptive_fec_for(source: Punktfunk1Source, static_override: bool) -> bool {
    !static_override
        && matches!(
            source,
            Punktfunk1Source::SyntheticAbr(_) | Punktfunk1Source::Virtual
        )
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
    sem: &Arc<tokio::sync::Semaphore>,
) -> Result<Result<tokio::sync::OwnedSemaphorePermit, punktfunk_core::reject::RejectReason>> {
    use punktfunk_core::reject::RejectReason;
    tracing::info!(name = %label, fingerprint = %fp_hex,
        "unpaired device knocked — parking connection for delegated approval in the console");
    // QUIC-validated source IP for the pending per-source cap. Knock generation makes
    // this connection the one an approval admits — siblings must not all start a session.
    let knock_seq = np.note_pending(label, fp_hex, Some(conn.remote_address().ip()));
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
            permit = match park_knock(&conn, Some(&mut send), np, &label, &fp_hex, &sem).await? {
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
        delivery_ask,
        compositor,
        gamescope_route,
        prep,
        joined,
        features,
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
        ),
    )
    .await
    .map_err(|_| anyhow!("handshake timed out after {HANDSHAKE_TIMEOUT:?}"))??;
    let (ctrl_send, ctrl_recv) = (send, recv);
    // The host's half of the path, for a client that asked: media leaves from the endpoint's
    // socket.
    let host_facts = delivery_ask
        .filter(|a| a.flags & punktfunk_core::quic::EXT_DELIVERY_FACTS != 0)
        .map(|_| crate::telemetry::net_health::host_facts(conn.v2().map(|v2| &*v2.media_socket)));
    // A diagnostic session: the stream thread serves probes and builds nothing.
    let probe_only =
        delivery_ask.is_some_and(|a| a.flags & punktfunk_core::quic::EXT_DELIVERY_PROBE_ONLY != 0);
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
    } = SessionWiring::new(&welcome, source);
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
    tokio::spawn(control::run(control::Task {
        ctrl_send,
        ctrl_recv,
        clock: conn.v2_session().clock.clone(),
        input_tx: input_tx.clone(),
        initial_mode: hello.mode,
        stream_config: features
            .has(punktfunk_core::quic::v2::registry::FEATURE_STREAM_CONFIG)
            .then_some(v2msg::StreamConfig {
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
            }),
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
        delivery_ask,
        host_facts,
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
        session_fp_hex.as_deref(),
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
    );

    // Handshake complete: CONNECTED. A client rejected earlier never emits either.
    emit_connected(
        &conn,
        crate::events::ClientRef {
            name: client_name.clone().unwrap_or_default(),
            fingerprint: session_fp_hex.clone(),
            plane: conn.plane(),
            preset: session_preset.clone(),
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
    });
    // Linux `PUNKTFUNK_PIN_CLOCKS`: refcounted vendor clock floor while any session streams.
    #[cfg(target_os = "linux")]
    let _clock_pin = crate::gpuclocks::session_pin();
    let launch_target = resolve_launch(hello.launch.as_deref(), &launch_outcome_tx).await?;
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
}

impl SessionPlanes {
    /// Minted after the handshake, before the input and audio threads.
    fn mint(
        joined: Option<&crate::vdisplay::admission::LiveDisplay>,
        compositor: Option<crate::vdisplay::Compositor>,
        route: Option<&crate::vdisplay::GamescopeRoute>,
        fp_hex: Option<&str>,
        inj_tx: &std::sync::mpsc::Sender<InputEvent>,
        mic_tx: std::sync::mpsc::SyncSender<crate::audio::MicFrame>,
    ) -> SessionPlanes {
        #[cfg(target_os = "linux")]
        {
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
                        // `--open` has no fingerprint; a per-accept sequence isolates at the cost
                        // of keep-alive.
                        static ANON_SEQ: AtomicU64 = AtomicU64::new(0);
                        let paired = fp_hex.map(seat_id);
                        let id = paired.clone().unwrap_or_else(|| {
                            format!("anon{}", ANON_SEQ.fetch_add(1, Ordering::Relaxed))
                        });
                        let iso = session_isolation(&id, paired.is_some());
                        tracing::info!(%id, sink = iso.sink.as_deref().unwrap_or("-"),
                            "isolated gamescope session — per-session input/audio/mic planes");
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
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (joined, compositor, route, fp_hex);
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

/// Live sessions, on either plane, that may stream a gamescope the host took over.
static LIVE_GAMESCOPE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Whether any session holds a [`GamescopeHold`]: a held takeover is then streaming, not kept.
pub(crate) fn gamescope_sessions_live() -> bool {
    LIVE_GAMESCOPE.load(Ordering::SeqCst) > 0
}

/// One count in [`LIVE_GAMESCOPE`], taken before the session resolves its compositor. Resolving
/// cancels a pending Game Mode hand-back; the last hold dropped, on any path, schedules it again.
pub(crate) struct GamescopeHold;

impl GamescopeHold {
    pub(crate) fn new() -> Self {
        LIVE_GAMESCOPE.fetch_add(1, Ordering::SeqCst);
        GamescopeHold
    }
}

impl Drop for GamescopeHold {
    fn drop(&mut self) {
        // A `join` session still shows the owner's game after the owner leaves.
        if LIVE_GAMESCOPE.fetch_sub(1, Ordering::SeqCst) == 1 {
            crate::vdisplay::restore_managed_session();
        }
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

/// This session's Steam home, or `None` for the box's own.
///
/// A seat is a fingerprint: an `anon<seq>` id is minted per accept, so a Steam signed in under
/// one would never be found again.
#[cfg(target_os = "linux")]
fn seat_home_for(paired: Option<&str>, on: bool) -> Option<std::path::PathBuf> {
    paired.filter(|_| on).map(pf_paths::seat_home)
}

/// The seat a device streams on: the head of its fingerprint. Short enough for a socket name,
/// wide enough that two paired devices do not collide. One function, because the pre-warm has to
/// name the same seat this session does or the registry hands its parked display to nobody.
#[cfg(target_os = "linux")]
fn seat_id(fp_hex: &str) -> String {
    fp_hex[..fp_hex.len().min(8)].to_string()
}

/// The isolated planes `id` streams on. `paired` says the id is a seat rather than an
/// `anon<seq>`, which is what earns a Steam home.
///
/// The registry's reuse key is `id` plus that home, so [`prewarm`] builds this value for a seat
/// before its client connects and the connect lands on the display already standing.
#[cfg(target_os = "linux")]
fn session_isolation(id: &str, paired: bool) -> crate::vdisplay::SessionIsolation {
    // Monitor-mode has no per-session sink — audio stays shared; input/mic still isolate.
    let sink =
        crate::audio::per_session_sink_possible().then(|| format!("punktfunk-speaker-iso-{id}"));
    let steam_home = seat_home_for(
        paired.then_some(id),
        pf_host_config::config().steam_seat_home,
    );
    crate::vdisplay::SessionIsolation::new(
        id.to_string(),
        sink,
        Some(format!("punktfunk-mic-{id}")),
        steam_home,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::v2::msg::V2Message;

    /// The knob is the only way in, and an unpaired session never gets a seat home.
    #[cfg(target_os = "linux")]
    #[test]
    fn only_a_paired_client_with_the_knob_on_gets_a_seat_home() {
        let seat = seat_home_for(Some("cafe0123"), true).expect("a paired seat has a home");
        assert!(seat.ends_with("seats/cafe0123"), "{}", seat.display());
        assert_eq!(seat_home_for(Some("cafe0123"), false), None, "knob off");
        assert_eq!(seat_home_for(None, true), None, "anon<seq> has no identity");
    }

    /// Adaptive FEC is offered only to a source that can keep encoder and packetizer
    /// on one wire budget: a proposal a source cannot apply would be accepted work
    /// that never reaches the wire.
    #[test]
    fn adaptive_fec_only_for_sources_with_a_coordinated_retarget() {
        let abr = Punktfunk1Source::SyntheticAbr(SynthAbrShape {
            content: Content::Steady { fill_pct: 100 },
            recovery: std::time::Duration::ZERO,
            answer: KeyframeAnswer::Idr,
            idr_pct: DEFAULT_IDR_PCT,
            bringup: std::time::Duration::ZERO,
            serve_ramp: false,
        });
        for (source, want) in [
            (Punktfunk1Source::Synthetic, false),
            (Punktfunk1Source::Software, false),
            (abr, true),
            (Punktfunk1Source::Virtual, true),
        ] {
            assert_eq!(
                adaptive_fec_for(source, false),
                want,
                "static override unset: {source:?}"
            );
            assert!(
                !adaptive_fec_for(source, true),
                "a pinned FEC adapts nothing: {source:?}"
            );
        }
    }

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

        let asleep = anyhow::Error::new(pf_vdisplay::DisplayAsleep)
            .context("acquire virtual output for the session (retry-hold lease)");
        let said = setup_failed_sentence(&asleep).expect("a dark display has words for the user");
        assert!(said.starts_with("The host's screen is asleep"), "{said}");
        assert!(said.len() <= punktfunk_core::quic::REFUSED_REASON_MAX);

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
        // A client rate is ignored; the host's bits per pixel sets the pin.
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
    fn pyrowave_pin_follows_the_host_bpp() {
        use crate::encode::ChromaFormat;
        use punktfunk_core::config::Mode;
        let mode = Mode {
            width: 3840,
            height: 2160,
            refresh_hz: 120,
        };
        let px = 3840 * 2160 * 120;
        // 0.5 bpp is Steam's 500 Mbps ceiling at 4K120.
        assert_eq!(
            pyrowave_pin_kbps(&mode, ChromaFormat::Yuv420, 8, 0.5),
            px / 2 / 1000
        );
        // 4:4:4 and 10-bit scale from the operator's value, not from 1.6.
        assert_eq!(
            pyrowave_pin_kbps(&mode, ChromaFormat::Yuv444, 10, 1.0),
            (f64::from(px) * 1.625 * 1.15 / 1000.0) as u32
        );
        let tiny = Mode {
            width: 64,
            height: 64,
            refresh_hz: 1,
        };
        assert_eq!(
            pyrowave_pin_kbps(&tiny, ChromaFormat::Yuv420, 8, 0.25),
            MIN_BITRATE_KBPS
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
        let pin = |requested, mode: &Mode, chroma, depth, ceiling: fn() -> Option<u32>| {
            resolve_bitrate_kbps_under(Codec::PyroWave, requested, mode, chroma, depth, ceiling)
        };
        fn none() -> Option<u32> {
            None
        }
        fn link() -> Option<u32> {
            Some(4_500_000)
        }
        let uncapped = pin(0, &mode, ChromaFormat::Yuv444, 10, none);
        assert!(
            uncapped > 5_000_000,
            "expected the open-loop pin, got {uncapped}"
        );
        // Ceiling caps the Automatic pin to the link rate.
        assert_eq!(pin(0, &mode, ChromaFormat::Yuv444, 10, link), 4_500_000);
        // A pin already under the ceiling is untouched.
        let small = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        assert_eq!(
            pin(0, &small, ChromaFormat::Yuv420, 8, link),
            1920 * 1080 * 60 * 16 / 10 / 1000
        );
        // Explicit client rate still goes through pin + ceiling.
        assert_eq!(
            pin(6_000_000, &mode, ChromaFormat::Yuv444, 10, link),
            4_500_000
        );
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
    unsafe fn pull_verified(conn: *mut punktfunk_ffi::PunktfunkConnection, count: u32) {
        use punktfunk_core::error::PunktfunkStatus;
        let mut got = 0u32;
        // SAFETY: `PunktfunkFrame` is `#[repr(C)]` POD; all-zero is valid (null `data`, `len == 0`).
        // Read only after `next_au` overwrites it on `Ok`.
        let mut frame = unsafe { std::mem::zeroed() };
        while got < count {
            // SAFETY: `conn` is the live handle from `punktfunk_connect` (caller asserts non-null,
            // does not close until after return). `&mut frame` outlives this call. This thread is
            // the only video puller.
            match unsafe { punktfunk_ffi::punktfunk_connection_next_au(conn, &mut frame, 2000) } {
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
    ///
    /// A session here also lands in the live registry, so every holder takes
    /// [`crate::session_status::tests::REGISTRY`] first — that order, always.
    static SESSION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// C ABI: TOFU connect → pull frames → send input → close. Three sequential sessions
    /// against one host prove the persistent listener; a wrong pin is rejected.
    #[test]
    fn c_abi_connection_roundtrip() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::error::PunktfunkStatus;
        use punktfunk_ffi::{
            punktfunk_connect, punktfunk_connection_close, punktfunk_connection_mode,
            punktfunk_connection_send_input,
        };

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
        let st = unsafe { punktfunk_ffi::punktfunk_connection_request_mode(conn, 1920, 1080, 144) };
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

    /// A `synthetic-abr` session publishes a registry row while it streams and retires it
    /// when it ends. The row's id is the one the control task reads off the session's
    /// counters before it asks the governor for a share, so a source that never registers
    /// leaves a shared path undivided. The row names the preset this dial carried.
    #[test]
    fn a_synthetic_abr_session_registers_while_it_streams() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::{ConnectParams, NativeClient};

        let host = std::thread::spawn(|| {
            run_ephemeral(Punktfunk1Options {
                port: 19782,
                source: Punktfunk1Source::SyntheticAbr(SynthAbrShape {
                    content: Content::Steady { fill_pct: 100 },
                    recovery: std::time::Duration::ZERO,
                    answer: KeyframeAnswer::Idr,
                    idr_pct: DEFAULT_IDR_PCT,
                    bringup: std::time::Duration::ZERO,
                    serve_ramp: false,
                }),
                seconds: 3,
                frames: 0, // this source is timed, not counted
                max_sessions: 1,
                max_concurrent: 1,
                require_pairing: false,
                allow_pairing: false,
                pairing_pin: None,
                paired_store: None,
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
        let client = NativeClient::connect(ConnectParams {
            preset: punktfunk_core::quic::SessionPreset::new("dock-1", "Docked"),
            ..ConnectParams::new("127.0.0.1", 19782, mode, std::time::Duration::from_secs(10))
        })
        .expect("client connects to the synthetic-abr host");

        // The registry is process-global and the session_status tests register their own
        // rows in it; this mode is what tells ours apart from theirs.
        let ours = || {
            crate::session_status::snapshot()
                .into_iter()
                .find(|s| (s.width, s.height, s.fps) == (1280, 720, 60))
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let row = loop {
            if let Some(r) = ours() {
                break r;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the synthetic-abr session never reached the registry"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert_ne!(
            row.id, 0,
            "0 is the id the control task skips the governor on"
        );
        assert_eq!(row.plane, crate::events::Plane::Native);
        assert_eq!(row.preset_name.as_deref(), Some("Docked"));

        drop(client);
        host.join().unwrap().unwrap();
        assert!(
            ours().is_none(),
            "the guard retires the row on the stream's exit path"
        );
    }

    /// Clipboard over a synthetic session: host advertises the cap, acks enable with
    /// `BACKEND_UNAVAILABLE` (no compositor), declines a fetch. Live-backend paths are
    /// not covered here. `design/clipboard-and-file-transfer.md`.
    #[test]
    fn clipboard_control_and_fetch_decline_over_session() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::{ConnectParams, NativeClient};
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
        let client = NativeClient::connect(ConnectParams::new(
            "127.0.0.1",
            19781,
            mode,
            std::time::Duration::from_secs(10),
        ))
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

    /// Spin up a host of `source` on `port` and dial it with `params`; the host joins on
    /// drop of the returned client.
    fn synthetic_session(
        port: u16,
        source: Punktfunk1Source,
        params: impl FnOnce(
            punktfunk_core::client::ConnectParams,
        ) -> punktfunk_core::client::ConnectParams,
    ) -> (
        punktfunk_core::client::NativeClient,
        std::thread::JoinHandle<anyhow::Result<()>>,
    ) {
        use punktfunk_core::client::{ConnectParams, NativeClient};
        let host = std::thread::spawn(move || {
            run_ephemeral(Punktfunk1Options {
                port,
                source,
                seconds: 0,
                frames: 600,
                max_sessions: 1,
                max_concurrent: 1,
                require_pairing: false,
                allow_pairing: false,
                pairing_pin: None,
                paired_store: None,
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
        let client = NativeClient::connect(params(ConnectParams::new(
            "127.0.0.1",
            port,
            mode,
            std::time::Duration::from_secs(10),
        )))
        .expect("client connects to synthetic host");
        (client, host)
    }

    fn wait_for(pred: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if pred() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    /// `EXT_TAG_DELIVERY` on `Start` is answered with the profile the session streams
    /// under and, when asked, the host's facts; a later `SetDelivery` is answered too.
    #[test]
    fn a_client_that_asks_for_a_profile_is_answered() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::quic::{DeliveryAsk, EXT_DELIVERY_FACTS, FORCED_PROFILE_NONE};
        let (client, host) = synthetic_session(19783, Punktfunk1Source::Synthetic, |p| {
            punktfunk_core::client::ConnectParams {
                delivery: Some(DeliveryAsk {
                    profile: 1,
                    flags: EXT_DELIVERY_FACTS,
                }),
                ..p
            }
        });
        assert!(
            wait_for(|| client.delivery().is_some()),
            "the host answers the tag"
        );
        let answer = client.delivery().unwrap();
        assert_eq!((answer.profile, answer.forced), (1, false));
        let facts = client
            .host_facts()
            .expect("facts follow the answer when asked");
        assert!(facts.sndbuf_kb > 0, "the data socket has a send buffer");
        assert_eq!(facts.forced_profile, FORCED_PROFILE_NONE);
        client.set_delivery(2).unwrap();
        assert!(
            wait_for(|| client.delivery().map(|d| d.profile) == Some(2)),
            "SetDelivery is answered"
        );
        drop(client);
        host.join().unwrap().unwrap();
    }

    /// A device that dials again while its first session still streams gets in once that
    /// session has released, not after a fixed grace: the old 1.5 s sleep is gone.
    #[test]
    fn a_reconnect_waits_for_the_release_not_a_timer() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::{ConnectParams, NativeClient};
        let host = std::thread::spawn(|| {
            run_ephemeral(Punktfunk1Options {
                port: 19793,
                source: Punktfunk1Source::Synthetic,
                seconds: 0,
                frames: 600,
                max_sessions: 2,
                max_concurrent: 2,
                require_pairing: false,
                allow_pairing: false,
                pairing_pin: None,
                paired_store: None,
                idle_timeout: None,
                mdns: false,
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        let (cert, key) = punktfunk_core::quic::endpoint::generate_identity().unwrap();
        let dial = || {
            NativeClient::connect(ConnectParams {
                identity: Some((cert.clone(), key.clone())),
                ..ConnectParams::new(
                    "127.0.0.1",
                    19793,
                    punktfunk_core::Mode {
                        width: 1280,
                        height: 720,
                        refresh_hz: 60,
                    },
                    std::time::Duration::from_secs(10),
                )
            })
            .expect("client connects")
        };
        let first = dial();
        assert!(first.next_frame(std::time::Duration::from_secs(5)).is_ok());
        let started = std::time::Instant::now();
        let second = dial();
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_millis(1400),
            "the reconnect took {took:?}"
        );
        assert!(second.next_frame(std::time::Duration::from_secs(5)).is_ok());
        assert!(
            wait_for(|| first.end_reason() != punktfunk_core::client::PunktfunkEndReason::None),
            "the first session was retired, not kept beside the second"
        );
        drop((first, second));
        host.join().unwrap().unwrap();
    }

    /// A client streams over `punktfunk/2`: the handshake crosses the translated control stream,
    /// the media arrives on the connection's own socket under exporter keys, and every frame is
    /// the host's byte for byte. Each frame's `HostTiming` names it by the
    /// session-clock pts it arrived with. Control round trips keep working.
    #[test]
    fn a_punktfunk_2_session_streams_end_to_end() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::quic::DeliveryAsk;
        let (client, host) = synthetic_session(19791, Punktfunk1Source::Synthetic, |p| {
            punktfunk_core::client::ConnectParams {
                delivery: Some(DeliveryAsk {
                    profile: 1,
                    flags: 0,
                }),
                ..p
            }
        });
        let mut got = 0;
        let mut pts = std::collections::HashSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while got < 60 && std::time::Instant::now() < deadline {
            if let Ok(f) = client.next_frame(std::time::Duration::from_millis(200)) {
                let idx = u32::from_le_bytes(f.data[0..4].try_into().unwrap());
                assert_eq!(f.data, test_frame(idx, f.data.len()), "frame {idx}");
                pts.insert(f.pts_ns);
                got += 1;
            }
        }
        assert_eq!(got, 60, "frames cross the v2 media path");
        let mut named = 0;
        while let Ok(t) = client.next_host_timing(std::time::Duration::from_millis(50)) {
            named += usize::from(pts.contains(&t.pts_ns));
        }
        assert!(named >= 50, "HostTiming names its frames: {named} of 60");
        assert!(
            wait_for(|| client.delivery().map(|d| d.profile) == Some(1)),
            "the host answers the delivery entry the ClientHello carried"
        );
        client.set_delivery(2).unwrap();
        assert!(
            wait_for(|| client.delivery().map(|d| d.profile) == Some(2)),
            "a control round trip crosses the translated stream"
        );
        drop(client);
        host.join().unwrap().unwrap();
    }

    /// A client that asks nothing hears nothing and may send nothing: the session streams
    /// as every shipped client's does.
    #[test]
    fn a_client_that_asks_nothing_streams_burst() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (client, host) = synthetic_session(19784, Punktfunk1Source::Synthetic, |p| p);
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert!(client.delivery().is_none(), "no tag, no answer");
        assert!(client.host_facts().is_none());
        assert!(matches!(
            client.set_delivery(1),
            Err(punktfunk_core::PunktfunkError::Unsupported(_))
        ));
        drop(client);
        host.join().unwrap().unwrap();
    }

    /// A `Start` that asks for probes only gets a session that serves every probe in full,
    /// back to back, and shows no video: nothing was built to show.
    #[test]
    fn a_probe_only_start_serves_probes_without_a_pipeline() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::quic::{DeliveryAsk, EXT_DELIVERY_PROBE_ONLY};
        let (client, host) = synthetic_session(19787, Punktfunk1Source::Synthetic, |p| {
            punktfunk_core::client::ConnectParams {
                delivery: Some(DeliveryAsk {
                    profile: 0,
                    flags: EXT_DELIVERY_PROBE_ONLY,
                }),
                ..p
            }
        });
        assert!(client.probe_only());
        assert!(
            wait_for(|| client.delivery().is_some()),
            "the tag is answered"
        );
        // Two long rounds back to back: a streaming session would clamp neither and
        // refuse the second for ten seconds.
        for _ in 0..2 {
            client.request_probe(20_000, 600).unwrap();
            assert!(wait_for(|| client.probe_result().done), "the round reports");
            let r = client.probe_result();
            assert!(r.wire_packets_sent > 0, "served, not declined");
            assert!(
                r.elapsed_ms >= 300,
                "served in full, not as a 50 ms step: {}",
                r.elapsed_ms
            );
        }
        assert!(matches!(
            client.next_frame(std::time::Duration::from_millis(500)),
            Err(punktfunk_core::PunktfunkError::NoFrame)
        ));
        drop(client);
        host.join().unwrap().unwrap();
    }

    /// The whole check over a probe-only session: the ramp proves a ceiling, the clean
    /// round runs under it, both shaped legs run back to back, and the host's facts arrive.
    #[test]
    fn the_network_check_runs_its_legs_over_a_probe_only_session() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::health::{self, LegShape};
        use punktfunk_core::quic::{DeliveryAsk, EXT_DELIVERY_FACTS, EXT_DELIVERY_PROBE_ONLY};
        let source = Punktfunk1Source::SyntheticAbr(SynthAbrShape {
            content: Content::Steady { fill_pct: 100 },
            recovery: std::time::Duration::ZERO,
            answer: KeyframeAnswer::Idr,
            idr_pct: DEFAULT_IDR_PCT,
            bringup: std::time::Duration::from_secs(2),
            serve_ramp: true,
        });
        let (client, host) =
            synthetic_session(19788, source, |p| punktfunk_core::client::ConnectParams {
                delivery: Some(DeliveryAsk {
                    profile: 0,
                    flags: EXT_DELIVERY_FACTS | EXT_DELIVERY_PROBE_ONLY,
                }),
                ..p
            });
        let r = health::health_check(&client, |_| {}).expect("the check reports");
        assert!(r.speed.clean.is_some(), "a ramp host gets a clean round");
        assert_eq!(
            r.legs.iter().map(|l| l.shape).collect::<Vec<_>>(),
            vec![LegShape::FrameBursts, LegShape::Capped]
        );
        for leg in &r.legs {
            assert!(
                leg.outcome.done && leg.outcome.wire_packets_sent > 0,
                "{leg:?}"
            );
        }
        let facts = r.host.expect("the host's facts arrived");
        assert!(facts.sndbuf_kb > 0);
        assert!(r.client.rcvbuf_kb > 0, "the client read its own grant");
        drop(client);
        host.join().unwrap().unwrap();
    }

    /// Toward a host without a ramp the speed test is the single blast, and says nothing
    /// about loss: there is no clean round to say it with.
    #[test]
    fn a_host_without_a_ramp_keeps_the_single_burst() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::health;
        let (client, host) = synthetic_session(19785, Punktfunk1Source::Synthetic, |p| p);
        assert_eq!(
            client.host_caps2() & punktfunk_core::quic::HOST_CAP2_RAMP,
            0,
            "the plain synthetic source serves no ramp"
        );
        let r = health::speed_test(&client, |_| {}).expect("the blast reports");
        assert!(r.clean.is_none(), "no ramp, no clean round");
        assert!(!r.wall);
        let blast = r.blast.expect("the blast's own reading stands");
        assert!(blast.done && blast.wire_packets_sent > 0);
        assert_eq!(r.ceiling_kbps, blast.throughput_kbps);
        drop(client);
        host.join().unwrap().unwrap();
    }

    /// Toward a host that serves the ramp, the ceiling is what the ramp proved and the clean
    /// round runs at half of it, with its own loss and jitter.
    #[test]
    fn the_clean_round_runs_under_the_ceiling_over_a_session() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::health;
        let source = Punktfunk1Source::SyntheticAbr(SynthAbrShape {
            content: Content::Steady { fill_pct: 100 },
            recovery: std::time::Duration::ZERO,
            answer: KeyframeAnswer::Idr,
            idr_pct: DEFAULT_IDR_PCT,
            bringup: std::time::Duration::from_secs(2),
            serve_ramp: true,
        });
        let (client, host) = synthetic_session(19786, source, |p| p);
        assert_ne!(
            client.host_caps2() & punktfunk_core::quic::HOST_CAP2_RAMP,
            0
        );
        let mut polls = 0u32;
        let r = health::speed_test(&client, |_| polls += 1).expect("the round reports");
        let clean = r.clean.expect("a ramp host gets a clean round");
        assert!(r.ceiling_kbps > 0, "the ramp proved a rate");
        assert_eq!(clean.rate_kbps, health::clean_rate_kbps(r.ceiling_kbps));
        // The figure itself is not asserted: a loopback ramp proves gigabits, and at that
        // rate this process's own receive buffer drops — the round measures the path it is
        // given.
        assert!(clean.outcome.done && clean.outcome.wire_packets_sent > 0);
        assert!(clean.outcome.recv_packets > 0);
        assert!(r.blast.is_none());
        assert!(polls > 0, "the round reported its progress");
        drop(client);
        host.join().unwrap().unwrap();
    }

    fn test_paired_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("punktfunk-paired-test-{}.json", std::process::id()))
    }

    /// Unpaired knock is parked; approve while waiting admits the same connection, no reconnect.
    #[test]
    fn delegated_approval_admits_after_knock() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::{ConnectParams, NativeClient};
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
        let expected_fp = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
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
        // No Hello name: assert the fingerprint-derived label. TOFU: approval, not a PIN,
        // authorizes this client.
        let client = NativeClient::connect(ConnectParams {
            identity: Some((cert, key)),
            ..ConnectParams::new("127.0.0.1", 19779, mode, std::time::Duration::from_secs(15))
        })
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
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::{ConnectParams, NativeClient};
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
            NativeClient::connect(ConnectParams::new("127.0.0.1", 19778, mode, timeout)).is_err(),
            "anonymous session must be rejected"
        );

        // 2: correct PIN → paired. The one online attempt consumes the window (step 4).
        let host_fp =
            NativeClient::pair("127.0.0.1", 19778, identity, "4321", "test-client", timeout)
                .expect("pairing with the right PIN");
        assert!(test_paired_path().exists());

        // 3: paired identity gets a session, pinned to the ceremony fingerprint.
        let client = NativeClient::connect(ConnectParams {
            pin: Some(host_fp),
            identity: Some((cert.clone(), key.clone())),
            ..ConnectParams::new("127.0.0.1", 19778, mode, timeout)
        })
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
    fn input_admission_matrix() {
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
            InputKind::Scroll,
            InputKind::TouchDown,
        ] {
            assert!(!admitted(GRANT_PRESET_CONTROLLER_ONLY, kind), "{kind:?}");
            assert!(!admitted(GRANT_PRESET_VIEW_ONLY, kind), "{kind:?}");
        }
        assert!(admitted(GRANT_ALL, InputKind::KeyDown));
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

    /// `ClientHello` → `ServerHello` → `Ready`, returning streams so a test can read
    /// `AccessUpdate`s and the exact close code. The media handle keeps the client's socket open.
    async fn raw_session(
        port: u16,
        identity: (&str, &str),
    ) -> (
        quinn::Connection,
        quinn::SendStream,
        punktfunk_core::quic::v2::io::FrameReader<quinn::RecvStream>,
        Welcome,
        punktfunk_core::transport::shared::ClientMedia,
    ) {
        use punktfunk_core::quic::v2::hello::{ClientHello, Ready, ServerHello};
        use punktfunk_core::quic::v2::{io as v2io, msg, registry};
        let (ep, _observed) = endpoint::client_shared(None, Some(identity), &[registry::ALPN]);
        let (ep, media) = ep.expect("client endpoint");
        let conn = ep
            .connect(format!("127.0.0.1:{port}").parse().unwrap(), "punktfunk")
            .expect("connect")
            .await
            .expect("QUIC handshake");
        let (mut send, recv) = conn.open_bi().await.expect("control stream");
        v2io::write_stream_type(&mut send, registry::STREAM_CONTROL)
            .await
            .expect("stream type");
        let mut recv = v2io::FrameReader::new(recv);
        let hello = Hello {
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
            audio_rate_hz: punktfunk_core::audio::SAMPLE_RATE_HZ,
            audio_bits: punktfunk_core::audio::pcm::BITS_16,
            audio_layout: 0,
            video_fit: 0,
        };
        let hello = ClientHello {
            hello,
            start_ext: Vec::new(),
            resume: None,
            suites: Vec::new(),
            features: Default::default(),
        };
        v2io::send(&mut send, &hello).await.expect("ClientHello");
        let welcome = loop {
            let (ty, body) = recv.read_frame().await.expect("ServerHello read");
            if ty != msg::Pending::TYPE {
                break msg::decode::<ServerHello>(ty, &body)
                    .expect("ServerHello")
                    .welcome;
            }
        };
        v2io::send(&mut send, &Ready {}).await.expect("Ready");
        (conn, send, recv, welcome, media)
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

    /// A client from before `punktfunk/2` still pairs by PIN over `pkf1`, and is closed with the
    /// wire-version code when it dials anything else there.
    #[test]
    fn a_pkf1_client_pairs_and_is_told_to_update() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::quic::{endpoint, pake, PairChallenge, PairProof, PairResult};

        let store = access_store_path("pkf1-pair");
        let _ = std::fs::remove_file(&store);
        let host = std::thread::spawn({
            let store = store.clone();
            move || {
                run_ephemeral(Punktfunk1Options {
                    port: 19784,
                    source: Punktfunk1Source::Synthetic,
                    seconds: 0,
                    frames: 25,
                    max_sessions: 2,
                    max_concurrent: 1,
                    require_pairing: true,
                    allow_pairing: false,
                    pairing_pin: Some("2468".into()),
                    paired_store: Some(store),
                    idle_timeout: None,
                    mdns: false,
                })
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        let (cert, key) = endpoint::generate_identity().unwrap();
        let client_fp = endpoint::fingerprint_of_pem(&cert).unwrap();
        let addr: std::net::SocketAddr = "127.0.0.1:19784".parse().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (ep, observed) = endpoint::client_pinned_offering(
                None,
                Some((cert.as_str(), key.as_str())),
                &[endpoint::QUIC_ALPN],
            );
            let ep = ep.expect("client endpoint");

            // The PIN ceremony in `punktfunk/1`'s framing, as such a client runs it.
            let conn = ep.connect(addr, "punktfunk").unwrap().await.unwrap();
            let host_fp = observed.lock().unwrap().expect("the host's certificate");
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            let (spake, spake_a) = pake::start(true, "2468", &client_fp, &host_fp);
            let req = PairRequest {
                name: "older client".into(),
                spake_a,
                device_key: Vec::new(),
            };
            pkf1::write(&mut send, &req.encode_pkf1()).await.unwrap();
            let challenge =
                PairChallenge::decode_pkf1(&pkf1::read(&mut recv).await.unwrap()).unwrap();
            let confirms = spake.finish(&challenge.spake_b).unwrap();
            assert!(pake::verify(&confirms.host, &challenge.confirm));
            let proof = PairProof {
                confirm: confirms.client,
            };
            pkf1::write(&mut send, &proof.encode_pkf1()).await.unwrap();
            let result = PairResult::decode_pkf1(&pkf1::read(&mut recv).await.unwrap()).unwrap();
            assert!(result.ok, "the pairing completes");
            conn.close(0u32.into(), b"pair done");

            // Its session dial is told to update. A v1 Hello opens with `PKF1`.
            let conn = ep.connect(addr, "punktfunk").unwrap().await.unwrap();
            let (mut send, _recv) = conn.open_bi().await.unwrap();
            pkf1::write(&mut send, b"PKF1\x01\x00\x00\x00")
                .await
                .unwrap();
            let code =
                tokio::time::timeout(std::time::Duration::from_secs(5), closed_app_code(&conn))
                    .await
                    .expect("the host closes the dial");
            assert_eq!(code, punktfunk_core::reject::WIRE_VERSION_CLOSE_CODE);
        });
        assert!(
            std::fs::read_to_string(&store).is_ok_and(|s| s.contains("older client")),
            "the paired device is stored"
        );
        let _ = std::fs::remove_file(&store);
        host.join().unwrap().unwrap();
    }

    /// Short expiry: Welcome advertises grants + remaining; deadline closes typed (`0x69`).
    #[test]
    fn access_expiry_advertises_and_closes_typed() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::quic::endpoint;

        let store = access_store_path("expiry");
        let _ = std::fs::remove_file(&store);
        let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
        let (cert, key) = endpoint::generate_identity().unwrap();
        let fp_hex = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
        np.add_with_access(
            "Evening Guest",
            &fp_hex,
            Some(crate::native_pairing::Access {
                grants: GRANT_ALL,
                expires_unix: Some(crate::clock::unix_secs() + 2),
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
            let (conn, _send, _recv, welcome, _media) =
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
        assert_eq!(np.effective(&fp_hex, crate::clock::unix_secs()), None);
        let _ = std::fs::remove_file(&store);
        host.join().unwrap().unwrap();
    }

    /// Mid-session grant edit → `AccessUpdate`; T−1 m warning fires; "expire now" typed-closes.
    #[test]
    fn access_edit_pushes_updates_and_expire_now_closes() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::quic::endpoint;

        let store = access_store_path("edit");
        let _ = std::fs::remove_file(&store);
        let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
        let (cert, key) = endpoint::generate_identity().unwrap();
        let fp_hex = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
        np.add("Edited Device", &fp_hex).unwrap();
        let host = spawn_access_host(19783, 1, np.clone());
        std::thread::sleep(std::time::Duration::from_millis(500));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (conn, _send, mut recv, welcome, _media) =
                raw_session(19783, (cert.as_str(), key.as_str())).await;
            assert_eq!(welcome.grants, GRANT_ALL);
            assert_eq!(welcome.expires_in_secs, 0, "permanent access advertises 0");

            // Controller-only, 62 s out (inside T−5 m, outside T−1 m): one warning, ~2 s later.
            let now = crate::clock::unix_secs();
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
            let (ty, body) =
                tokio::time::timeout(std::time::Duration::from_secs(5), recv.read_frame())
                    .await
                    .expect("edit AccessUpdate owed")
                    .expect("control stream open");
            let u = punktfunk_core::quic::v2::msg::decode::<AccessUpdate>(ty, &body)
                .expect("an AccessUpdate");
            assert_eq!(u.grants, punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY);
            assert!(
                (55..=62).contains(&u.remaining_secs),
                "remaining should track the fresh deadline, got {}",
                u.remaining_secs
            );

            // Update 2: T−1 m warning, fired as the threshold is crossed live.
            let (ty, body) =
                tokio::time::timeout(std::time::Duration::from_secs(10), recv.read_frame())
                    .await
                    .expect("T-1m warning owed")
                    .expect("control stream open");
            let u = punktfunk_core::quic::v2::msg::decode::<AccessUpdate>(ty, &body)
                .expect("an AccessUpdate");
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
                    expires_unix: Some(crate::clock::unix_secs() - 1),
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
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::{ConnectParams, NativeClient};
        use punktfunk_core::quic::endpoint;

        let store = access_store_path("launch");
        let _ = std::fs::remove_file(&store);
        let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
        let (cert, key) = endpoint::generate_identity().unwrap();
        let fp_hex = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
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
        let refused = NativeClient::connect(ConnectParams {
            launch: Some("steam:570".into()),
            name: Some("Guest Pad".into()),
            identity: Some((cert.clone(), key.clone())),
            ..ConnectParams::new("127.0.0.1", 19784, mode, timeout)
        });
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
        let client = NativeClient::connect(ConnectParams {
            name: Some("Guest Pad".into()),
            identity: Some((cert, key)),
            ..ConnectParams::new("127.0.0.1", 19784, mode, timeout)
        })
        .expect("controller-only session without a launch must be admitted");
        drop(client);
        let _ = std::fs::remove_file(&store);
        host.join().unwrap().unwrap();
    }

    /// A launch the host cannot resolve streams on, and the client learns why from the
    /// control message.
    #[test]
    fn unknown_launch_reaches_the_client_as_a_refusal() {
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::{ConnectParams, NativeClient};
        use punktfunk_core::quic::{endpoint, LaunchOutcomeKind};

        let store = access_store_path("launch-outcome");
        let _ = std::fs::remove_file(&store);
        let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
        let (cert, key) = endpoint::generate_identity().unwrap();
        let fp_hex = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
        np.add_with_access("Launcher", &fp_hex, None).unwrap();
        let host = spawn_access_host(19786, 1, np);
        std::thread::sleep(std::time::Duration::from_millis(500));
        let client = NativeClient::connect(ConnectParams {
            launch: Some("pf-test:no-such-title".into()),
            name: Some("Launcher".into()),
            identity: Some((cert, key)),
            ..ConnectParams::new(
                "127.0.0.1",
                19786,
                punktfunk_core::Mode {
                    width: 1280,
                    height: 720,
                    refresh_hz: 60,
                },
                std::time::Duration::from_secs(10),
            )
        })
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
        let _registry = crate::session_status::tests::registry_lock();
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::{ConnectParams, NativeClient};
        use punktfunk_core::quic::endpoint;

        let store = access_store_path("regrant");
        let _ = std::fs::remove_file(&store);
        let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
        let (cert, key) = endpoint::generate_identity().unwrap();
        let fp_hex = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
        // Still listed, no longer authorized.
        np.add_with_access(
            "Yesterday's Guest",
            &fp_hex,
            Some(crate::native_pairing::Access {
                grants: GRANT_ALL,
                expires_unix: Some(crate::clock::unix_secs() - 3600),
                until_disconnect: false,
            }),
        )
        .unwrap();
        assert!(np.is_paired(&fp_hex), "expired but still listed");
        assert_eq!(np.effective(&fp_hex, crate::clock::unix_secs()), None);

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
                        expires_unix: Some(crate::clock::unix_secs() + 4 * 3600),
                        until_disconnect: false,
                    }),
                )
                .unwrap()
                .paired()
                .expect("re-approval");
        });

        let client = NativeClient::connect(ConnectParams {
            name: Some("Yesterday's Guest".into()),
            identity: Some((cert, key)),
            ..ConnectParams::new(
                "127.0.0.1",
                19785,
                punktfunk_core::Mode {
                    width: 1280,
                    height: 720,
                    refresh_hz: 60,
                },
                std::time::Duration::from_secs(15),
            )
        })
        .expect("re-approved mid-park → session admitted with no reconnect");
        approver.join().unwrap();
        // Re-grant in force: controller-only.
        assert_eq!(
            np.effective(&fp_hex, crate::clock::unix_secs()),
            Some(punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY)
        );
        drop(client);
        let _ = std::fs::remove_file(&store);
        host.join().unwrap().unwrap();
    }
}
