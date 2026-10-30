//! Native host plane: `pkf2` sessions over QUIC; `pkf1` only pairs an older client. Video,
//! audio and input ride the connection's datagrams; the data planes are threads, not async.
//! Concurrent sessions share the host-lifetime injector, mic and audio capture.
//!
//! A connection runs `serve` → `serve_connection` → admission → `run_admitted`, which runs the
//! session as phases under one `SessionScope` (`scope`): `handshake` settles the terms from one
//! `Negotiation`, `wiring` builds the channels, then the planes start: `control` (the control
//! stream and its rate table), `stream` (the video loop), `input` and `audio` (`planes`), and
//! the launch (`launch`).
//!
//! The rules:
//! - Every thread a session starts is its scope's. Teardown joins them in one order, on every
//!   exit, and a session that fails before it streams leaves the typed close to its plane.
//! - A pipeline rebuild is planned (`stream::rebuild_plan`) and performed in one place; a site
//!   only names its ask.
//! - Knobs are read once (`knobs`): the process env at start, the console's rows per session.
//!
//! Serves `~/.config/punktfunk/native-cert.pem` (shared with the mgmt API) and logs the SHA-256
//! fingerprint clients pin. Tests: `native/tests.rs`.

use anyhow::{anyhow, Context, Result};
use punktfunk_core::config::{CompositorPref, FecConfig, FecScheme, GamepadPref, Role};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::packet::{FLAG_PIC, FLAG_PROBE, FLAG_SOF};
use punktfunk_core::quic::v2::hello::{ClientHello, Ready, ServerHello};
use punktfunk_core::quic::v2::msg as v2msg;
use punktfunk_core::quic::{
    classify, endpoint, pkf1, AccessUpdate, AckReason, BitrateChanged, ClockEcho, ClockProbe,
    GrantClass, Hello, PairRequest, PipelineGap, ProbeResult, ProbeShaped, Reconfigure,
    Reconfigured, SetBitrate, Welcome, GRANT_ALL, GRANT_CLIPBOARD, GRANT_LAUNCH,
};
use punktfunk_core::session::test_frame;
use punktfunk_core::Session;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

// The session's control connection, whichever transport carries it (quinn or WebTransport).
pub(crate) mod link;
use crate::compositor_route::resolve_compositor;

/// GameStream presents the same virtual pad and must pick `windows_xbox_hid` from this definition.
pub(crate) mod gamepad;
use gamepad::{resolve_gamepad, resolve_pad_kind, route_decision};

mod pairing;
pub(crate) use pairing::{pair_ceremony, PairWire};

/// The session's expiry clock and live grant watch.
mod access;
use access::{access_lifecycle, remaining_secs_wire};

mod audio;

/// Bitrate and FEC policy: resolved rates, the PyroWave pin, the encoder ceiling, adaptive FEC.
mod bitrate;
use bitrate::{adaptive_fec_for, audio_reserved_kbps};

/// Per-pad DualSense audio (0xD1 → `PAD_AUDIO_MAGIC`). The input thread spawns one
/// streamer per pad; Welcome advertises the cap via `pad_audio::host_cap`.
mod pad_audio;

mod input;
/// Per-pad motion inter-arrival ([`motion_cadence::MotionCadence`]), logged at session end.
mod motion_cadence;
/// Controller updates reaching the host ([`pad_uplink::PadUplink`]): the client → host link.
mod pad_uplink;
use input::ClientInput;

mod handshake;
pub(crate) use handshake::redirect;
/// `PUNKTFUNK_WIRE_MTU`, the control-connection path-MTU watch, and the per-peer shard clamp.
mod wire_mtu;

mod control;
mod cursor_fwd;

mod stream;
#[cfg(feature = "gamestream")]
pub(crate) use stream::shared as pipeline;
use stream::{
    reconfig_allowed, software_stream, synthetic_abr_stream, synthetic_stream, virtual_stream,
    SessionContext, StreamCommon, SynthAbrContext,
};
mod wiring;
pub use stream::{Content, KeyframeAnswer, SynthAbrShape, DEFAULT_IDR_PCT};
use wiring::SessionWiring;
/// Every `PUNKTFUNK_*` read of the plane.
mod knobs;
pub(crate) use knobs::HostKnobs;
use knobs::SessionKnobs;
mod scope;
use scope::SessionScope;
mod planes;
use planes::PlaneEnds;
mod launch;
use launch::Launch;

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
    profiles.share_light_seats();
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

/// A profile store in a scratch dir: tests never read or write the real `profiles.json`.
#[cfg(test)]
pub(crate) fn test_profiles() -> Arc<crate::profiles::Profiles> {
    let path = crate::test_support::scratch().join("profiles.json");
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

pub(crate) fn native_serve_opts(cfg: &NativeServe) -> Punktfunk1Options {
    let knobs = HostKnobs::get();
    Punktfunk1Options {
        port: cfg.port,
        source: knobs.source,
        seconds: 7 * 24 * 3600, // 7 days: a cap, not a cut of a live stream
        frames: 0,
        max_sessions: 0,
        max_concurrent: DEFAULT_MAX_CONCURRENT,
        require_pairing: cfg.require_pairing,
        allow_pairing: false,
        pairing_pin: None,
        paired_store: None,
        idle_timeout: knobs.idle_timeout,
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
    hold_guests_kept_at_start(&np);
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

    // Held for the host lifetime: dropping `_advert` unregisters.
    let _advert = advertise(&opts, mgmt_port, &fingerprint);

    // The door places every connect and runs no session, so it opens no audio, input or display
    // and the senders below reach nobody.
    let door = pf_paths::seat::role().is_door();
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
    // Failure logs once and leaves pads working without pad audio. A seat's pad endpoints come
    // from its listener's enumerator; the box-wide ones are the console host's.
    #[cfg(target_os = "windows")]
    if !pf_paths::seat::role().is_seat_host() {
        crate::audio::pad_endpoint::provision_at_startup(true);
    }
    // Windows: mint "Punktfunk Speakers/Microphone" (Valve streaming drivers). Best-effort;
    // without Steam's drivers the wiring plan keeps its name-based ladder.
    #[cfg(target_os = "windows")]
    crate::audio::minted::provision_at_startup();
    // Debounced TV-session restore on idle, not per-disconnect. Dropping this stops it.
    let _restore_worker = (!door).then(crate::vdisplay::start_restore_worker);
    if !door {
        // Take back the desk changes a crashed previous instance journaled; its gamescope
        // takeover is adopted, then swept and handed back by the startup restore.
        crate::vdisplay::reclaim_topology_journal();
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
    let opts = Arc::new(opts);
    if np.follows_box() {
        spawn_box_watch(np.clone(), profiles.clone());
    }

    // Permit taken before accept: overflow waits in QUIC's backlog. `0` = unlimited.
    // Handshake + pipeline run in the spawned task so a slow client never blocks accept.
    let permits = match opts.max_concurrent {
        0 => tokio::sync::Semaphore::MAX_PERMITS,
        n => n,
    };
    let sem = Arc::new(tokio::sync::Semaphore::new(permits));
    let host = || SessionHost {
        opts: Arc::clone(&opts),
        audio_cap: audio_cap.clone(),
        inj_tx: inj_sender(),
        mic_tx: mic_sender(),
        np: np.clone(),
        profiles: profiles.clone(),
        stats: stats.clone(),
        knobs: HostKnobs::get(),
    };
    // Secondary tier: a port it cannot bind must not take the streaming host down. Loud, though.
    if let Some(plane) = web {
        tokio::spawn(crate::webtransport::run(plane, host(), sem.clone()));
    }
    let mut sessions = tokio::task::JoinSet::new();
    let at = Arc::new(Accepting {
        fingerprint,
        media_socket,
        last_pairing: std::sync::Mutex::new(None),
        sem,
        accepted: std::sync::atomic::AtomicU32::new(0),
        done: tokio::sync::Notify::new(),
        max_sessions: opts.max_sessions,
    });
    tracing::info!(
        max_concurrent = opts.max_concurrent,
        "accepting sessions (concurrent)"
    );
    loop {
        // A finished task stays in the set until joined; a serving host never reaches the drain
        // below, and every client probe is a task.
        while sessions.try_join_next().is_some() {}
        let incoming = tokio::select! {
            i = ep.accept() => match i {
                Some(i) => i,
                None => break,
            },
            () = at.done.notified(), if at.max_sessions != 0 => break,
        };
        // A source that has not proved it can receive at the address it claims gets a Retry
        // rather than a task and a TLS handshake, so a spoofed-source flood cannot make this
        // host amplify. Costs the real client one RTT on first contact. A failed retry drops
        // `Incoming`, which refuses.
        if !incoming.remote_address_validated() {
            let _ = incoming.retry();
            continue;
        }
        sessions.spawn(serve_connection(incoming, host(), at.clone()));
    }
    // Drain in-flight sessions (max_sessions reached or endpoint closed).
    while sessions.join_next().await.is_some() {}
    ep.wait_idle().await;
    Ok(())
}

/// The `_punktfunk._udp` advert. None on a seat host, with `--no-mdns`, or when discovery fails:
/// a client still connects by address.
fn advertise(
    opts: &Punktfunk1Options,
    mgmt_port: u16,
    fingerprint: &[u8; 32],
) -> Option<crate::discovery::Advert> {
    if pf_paths::seat::role().is_seat_host() {
        tracing::info!("seat host: no mDNS advertisement — clients arrive through the box");
        return None;
    }
    if !opts.mdns {
        tracing::info!(
            "mDNS advertisement disabled (--no-mdns / PUNKTFUNK_MDNS) — clients connect by address"
        );
        return None;
    }
    let h = crate::host::Host::detect()
        .map_err(|e| tracing::warn!(error = %format!("{e:#}"), "host detect for mDNS failed (continuing)"))
        .ok()?;
    crate::discovery::advertise_native(
        &h.hostname,
        opts.port,
        &hex::encode(fingerprint),
        opts.require_pairing,
        &h.uniqueid,
        // 0 = standalone (no mgmt API) → do not advertise an `mgmt` port.
        (mgmt_port != 0).then_some(mgmt_port),
        &h.os_chain,
    )
    .map_err(
        |e| tracing::warn!(error = %format!("{e:#}"), "native mDNS advertise failed (continuing)"),
    )
    .ok()
}

/// What the accept loop shares with every connection's task.
struct Accepting {
    fingerprint: [u8; 32],
    media_socket: Arc<std::net::UdpSocket>,
    last_pairing: std::sync::Mutex<Option<std::time::Instant>>,
    /// The session slots, shared with the browser plane.
    sem: Arc<tokio::sync::Semaphore>,
    /// Handshakes that completed. `--max-sessions` counts those, not attempts, so the task that
    /// finishes the N-th one wakes the accept loop through `done`.
    accepted: std::sync::atomic::AtomicU32,
    done: tokio::sync::Notify,
    max_sessions: u32,
}

/// One connection, off the accept loop: a peer that stalls its handshake holds only its own task,
/// up to the idle timeout. A pin mismatch ends here, before a session slot is taken; a `pkf1`
/// client is answered on the pairing path; a session's error closes typed.
async fn serve_connection(incoming: quinn::Incoming, host: SessionHost, at: Arc<Accepting>) {
    let conn = match incoming.await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "QUIC accept failed");
            return;
        }
    };
    let n = at
        .accepted
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 1;
    if at.max_sessions != 0 && n >= at.max_sessions {
        at.done.notify_one();
    }
    let peer = conn.remote_address();
    if endpoint::negotiated_alpn(&conn).as_deref() != Some(punktfunk_core::quic::v2::registry::ALPN)
    {
        serve_pkf1(
            conn,
            at.media_socket.clone(),
            &at.fingerprint,
            &host.np,
            &at.last_pairing,
        )
        .await;
        return;
    }
    tracing::info!(%peer, "client connected");
    let conn_err = conn.clone();
    let link = link::SessionLink::QuicV2(
        conn.clone(),
        Arc::new(link::V2Link::new(conn, at.media_socket.clone())),
    );
    let SessionHost {
        opts,
        audio_cap,
        inj_tx,
        mic_tx,
        np,
        profiles,
        stats,
        ..
    } = host;
    // `serve_session` takes the slot once the peer has spoken: released while a knock is parked,
    // re-acquired on approval.
    match serve_session(
        link,
        &opts,
        &audio_cap,
        inj_tx,
        mic_tx,
        &at.fingerprint,
        &np,
        &profiles,
        &at.last_pairing,
        stats,
        at.sem.clone(),
    )
    .await
    {
        Ok(Served::Session) => tracing::info!(%peer, "session complete"),
        Ok(Served::ProbeClose) => tracing::debug!(
            %peer,
            "closed before the control handshake (reachability probe)"
        ),
        Ok(Served::Management) => tracing::debug!(%peer, "management connection closed"),
        Ok(Served::Access) => tracing::info!(%peer, "access request answered"),
        Err(e) => {
            // Typed setup-failed close so the client does not see a bare mid-frame drop.
            // First-wins: a gate that already closed, or a peer close, makes this a no-op. The
            // reason bytes are read by a person: the user sentence; the chain stays in the log.
            let detail = format!("{e:#}");
            conn_err.close(
                punktfunk_core::reject::SETUP_FAILED_CLOSE_CODE.into(),
                setup_failed_sentence(&e).unwrap_or_default().as_bytes(),
            );
            tracing::warn!(%peer, error = %detail, "session ended with error")
        }
    }
}

/// Shutdown wait for the box's session to come back. Bounds a wedge; well inside systemd's
/// 90 s `TimeoutStopSec`.
#[cfg_attr(windows, allow(dead_code, reason = "the signal handler is Unix only"))]
const SHUTDOWN_RESTORE_GRACE: std::time::Duration = std::time::Duration::from_secs(20);

/// A stop asked from inside: `host.restart` on a host its unit or runner starts over.
static STOP: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Stops this host as its stop signal would: the box is given back first.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn stop_self() -> Result<(), String> {
    STOP.notify_one();
    Ok(())
}

/// Catch `SIGTERM`/`SIGINT` or [`stop_self`], end the live sessions, give the box back, then
/// exit. `exit(0)` runs no destructor, so this is where the audio defaults, every display's
/// topology restore and output, and a Game Mode takeover are undone. Crash-restore lives in
/// `$XDG_RUNTIME_DIR`, which logind removes with the user manager. Blocking, under
/// [`SHUTDOWN_RESTORE_GRACE`].
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
            _ = STOP.notified() => "host.restart",
        };
        tracing::info!(
            signal = sig,
            "host stopping — handing the box's session back"
        );
        // Sessions end first, as a power action ends them: each client hears a typed close
        // instead of freezing on a display torn down under it.
        crate::power::set_closing(true);
        crate::session_status::stop_all_quit();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
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

/// A seat host's pairing store and profiles are the box's files, edited in another process.
/// Once a second its live sessions hear what changed in them: a grant narrowed, expired or
/// revoked, and a profile removed.
fn spawn_box_watch(np: Arc<NativePairing>, profiles: Arc<crate::profiles::Profiles>) {
    tokio::spawn(async move {
        let mut known = std::collections::HashSet::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tick.tick().await;
            np.republish_watched();
            profiles.announce_removals(&mut known);
        }
    });
}

/// Ends the session as an operator stop when the console removes the profile it plays as.
/// The end is written before the refuse, or the connection watcher would read the host's
/// own close back as `lost`.
fn spawn_profile_watch(scope: &SessionScope, profile: String) {
    let (conn, life) = (scope.conn().clone(), scope.life().clone());
    let mut removed = crate::profiles::removed();
    scope.task(async move {
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
        life.end(crate::events::SessionEndReason::StoppedByOperator);
        let reason = punktfunk_core::reject::RejectReason::SeatUnavailable;
        conn.refuse(
            reason.close_code(),
            "Your profile was removed by the host's owner.",
        )
        .await;
    });
}

/// Close with the typed reject code before the session task returns `Err`. A bare drop
/// closes with code 0, which the client cannot tell from transport trouble.
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

/// [`wait_for_approval`], then a session slot like any fresh client's (waits if busy).
///
/// `Ok(Ok(_))` is an approval with its slot. `Ok(Err(reason))` is the refusal to send.
/// `Err` means the client left before a decision.
pub(crate) async fn park_knock(
    conn: &link::SessionLink,
    send: Option<&mut link::CtlSend>,
    np: &NativePairing,
    label: &str,
    fp_hex: &str,
    profile: Option<&str>,
    sem: &Arc<tokio::sync::Semaphore>,
) -> Result<Result<tokio::sync::OwnedSemaphorePermit, punktfunk_core::reject::RejectReason>> {
    if let Err(reason) = wait_for_approval(conn, send, np, label, fp_hex, profile).await? {
        return Ok(Err(reason));
    }
    tracing::info!(name = %label, fingerprint = %fp_hex,
        "device approved in console — admitting session (no reconnect)");
    Ok(Ok(slot_while_connected(conn, sem).await?))
}

/// A session slot, waited for only while the client is still there. A waiter that left must not
/// take a slot, and with it, under `mode_conflict = steal`, another session's display.
async fn slot_while_connected(
    conn: &link::SessionLink,
    sem: &Arc<tokio::sync::Semaphore>,
) -> Result<tokio::sync::OwnedSemaphorePermit> {
    tokio::select! {
        biased;
        _ = conn.closed() => anyhow::bail!("client disconnected while waiting for a session slot"),
        p = sem.clone().acquire_owned() => Ok(p.expect("session semaphore is never closed")),
    }
}

/// Park an unpaired knock until the console decides, holding no session slot. A
/// `punktfunk/2` client hears `Pending` on `v2` meanwhile, every [`PENDING_EVERY`].
///
/// `Ok(Ok(()))` is an approval. `Ok(Err(reason))` is the refusal to send. `Err` means the
/// client left before a decision.
pub(crate) async fn wait_for_approval(
    conn: &link::SessionLink,
    mut send: Option<&mut link::CtlSend>,
    np: &NativePairing,
    label: &str,
    fp_hex: &str,
    profile: Option<&str>,
) -> Result<Result<(), punktfunk_core::reject::RejectReason>> {
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
                // Bounded: a peer that stops reading would hold this arm, and the decision and
                // close arms with it, past every wait.
                if let Some(w) = send.as_deref_mut() {
                    let sent = punktfunk_core::quic::v2::io::send(w, &v2msg::Pending {});
                    if !matches!(tokio::time::timeout(PENDING_EVERY, sent).await, Ok(Ok(_))) {
                        anyhow::bail!("client stopped reading while its pairing approval was pending");
                    }
                }
            }
        }
    };
    let reason = match decision {
        PairingDecision::Approved => return Ok(Ok(())),
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
    /// An access-only hello, answered without a session.
    Access,
}

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
    #[cfg(target_os = "linux")]
    if let Some(n) = e.downcast_ref::<pf_vdisplay::NoVirtualOutputs>() {
        return Some(n.user_message().into());
    }
    // Umbriel is the one Linux backend whose outputs have no PipeWire node.
    #[cfg(target_os = "linux")]
    if e.downcast_ref::<pf_capture::NoPipeWireNode>().is_some() {
        return Some(
            "This Umbriel session can't be captured directly; the stream needs a GPU encoder."
                .into(),
        );
    }
    e.downcast_ref::<pf_vdisplay::DisplayAsleep>()
        .map(|d| d.user_message())
}

/// Handshake → input/audio → data plane. RAII teardown. A first-message PairRequest is
/// the pairing ceremony instead, and an access-only hello ends at the pairing gate.
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
    // live path (keep-alive) instead of a silent dial timeout. An access-only hello never
    // takes one, so a busy host or seat cannot turn it away.
    let access_only = first.access_only;
    let mut permit = match access_only {
        true => None,
        false => Some(slot_while_connected(&conn, &sem).await?),
    };
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
            let decision = if access_only {
                wait_for_approval(&conn, Some(&mut send), np, &label, &fp_hex, asked)
                    .await?
                    .map(|()| None)
            } else {
                park_knock(&conn, Some(&mut send), np, &label, &fp_hex, asked, &sem)
                    .await?
                    .map(Some)
            };
            permit = match decision {
                Ok(permit) => permit,
                Err(reason) => {
                    close_rejected(&conn, reason).await;
                    anyhow::bail!("pairing request refused: {reason}");
                }
            };
        }
    }
    let Some(permit) = permit else {
        conn.refuse(
            punktfunk_core::reject::ACCESS_GRANTED_CLOSE_CODE,
            "access granted",
        )
        .await;
        return Ok(Served::Access);
    };
    // Admitted. From here the session is the same on every carrier.
    let host = SessionHost {
        opts: Arc::clone(opts),
        audio_cap: audio_cap.clone(),
        inj_tx,
        mic_tx,
        np: np_arc.clone(),
        profiles: profiles.clone(),
        stats,
        knobs: HostKnobs::get(),
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
    pub(crate) knobs: &'static HostKnobs,
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
            knobs: HostKnobs::get(),
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
/// [`link::SessionLink::peer_fingerprint`], whichever plane admitted it. Past admission it runs
/// as phases over one [`SessionScope`]: negotiate, wire, planes, launch, stream.
pub(crate) async fn run_admitted(
    conn: link::SessionLink,
    send: link::CtlSend,
    recv: link::CtlReader,
    first: ClientHello,
    host: &SessionHost,
    data_plane: DataPlane,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Served> {
    let (mut send, mut recv) = (send, recv);
    let Some((mut who, resolved)) = admit_here(host, &conn, &mut send, &first).await? else {
        // Placed on its seat's host: the redirect was this connect's session.
        return Ok(Served::Session);
    };
    // Born before the handshake, so a Welcome-time display prep aborts if the client vanishes.
    let life = crate::lifecycle::Lifecycle::new();
    let mut scope = SessionScope::new(life, conn);
    // Held first, so the slot comes back last.
    scope.hold(permit);
    scope.hold(who.count.take());
    spawn_end_watch(&scope);
    spawn_profile_watch(&scope, resolved.id.clone());
    scope.life().enter(crate::lifecycle::Phase::Negotiating);
    // Every phase's error returns here, so every exit reaches the one teardown.
    let result: Result<()> = async {
        let agreed = negotiate_session(
            &mut scope,
            (&mut send, &mut recv),
            &first,
            host,
            &who,
            resolved,
        )
        .await?;
        let shared_plane = matches!(data_plane, DataPlane::Shared(_));
        let (wired, ends) = wire_session(
            &mut scope,
            (send, recv),
            host,
            &mut who,
            &agreed,
            shared_plane,
        )
        .await;
        planes::spawn_planes(&mut scope, host, &who, &agreed, &wired, ends)?;
        let outcome = &wired.launch_outcome_tx;
        let launch =
            launch::launch_session(&mut scope, &who, &agreed, &wired.controls, outcome).await?;
        stream_session(&mut scope, host, who, agreed, wired, launch, data_plane).await
    }
    .await;
    scope.finish(&result).await;
    result.map(|()| Served::Session)
}

/// What admission settled for one session. Every phase after it reads this.
struct Admitted {
    fp_hex: Option<String>,
    grants: u32,
    deadline_unix: Option<i64>,
    /// The trust record's watch, until the access task takes it.
    access_watch: Option<tokio::sync::watch::Receiver<crate::native_pairing::AccessState>>,
    expires_in_secs: u32,
    profile_ref: crate::events::ProfileRef,
    /// This device's session count, until the scope holds it.
    count: Option<crate::native_pairing::SessionCountGuard>,
    knobs: SessionKnobs,
    /// Hello in hand; send thread finishes this when the first video packet leaves.
    bringup: Arc<crate::bringup::Trace>,
    /// Session totals for the summary. The input, audio and encode paths all outlive the
    /// guard that reads them, so they bump a shared block rather than hand a figure over.
    counters: Arc<crate::session_status::SessionCounters>,
}

/// The trust record, the profile and its placement. `None` once the profile was sent to play
/// on its seat's host.
async fn admit_here(
    host: &SessionHost,
    conn: &link::SessionLink,
    send: &mut link::CtlSend,
    first: &ClientHello,
) -> Result<Option<(Admitted, crate::profiles::Resolved)>> {
    let fp_hex = conn.peer_fingerprint().map(hex::encode);
    let Admission {
        grants,
        deadline_unix,
        watch,
        at_unix,
        session,
    } = admit(host, fp_hex.as_deref(), conn, first).await?;
    // Before anything is built for it: an unknown profile closes with no display touched.
    let resolved = match host
        .profiles
        .resolve(fp_hex.as_deref(), first.profile.as_deref())
    {
        Ok(r) => r,
        Err(e) => {
            use punktfunk_core::reject::RejectReason;
            let reason = match e {
                crate::profiles::ProfileError::Unknown
                | crate::profiles::ProfileError::NotThisSeat => RejectReason::ProfileUnknown,
                crate::profiles::ProfileError::SessionUnavailable => RejectReason::SeatUnavailable,
            };
            close_rejected(conn, reason).await;
            anyhow::bail!("profile refused: {e:?}");
        }
    };
    tracing::info!(profile = %resolved.id, name = %resolved.display_name, via = ?resolved.via, "profile");
    // A refusal ends the connect here and leaves the profile's last-used time alone.
    let placed = place_profile(conn, send, fp_hex.clone(), first, &resolved).await?;
    host.profiles.touch(&resolved.id);
    if placed.is_some() {
        if let (Some(guard), Some(fp)) = (session, fp_hex) {
            if host.np.is_session_only(&fp) {
                hold_while_on_seat(guard, fp, false);
            }
        }
        return Ok(None);
    }
    let profile_ref = crate::events::ProfileRef {
        id: resolved.id.clone(),
        display_name: resolved.display_name.clone(),
    };
    let who = Admitted {
        fp_hex,
        grants,
        deadline_unix,
        access_watch: watch,
        expires_in_secs: remaining_secs_wire(deadline_unix, at_unix),
        profile_ref,
        count: session,
        knobs: SessionKnobs::read(),
        bringup: crate::bringup::Trace::start("bringup", Arc::new(AtomicU32::new(0))),
        counters: Arc::new(crate::session_status::SessionCounters::default()),
    };
    Ok(Some((who, resolved)))
}

/// What the handshake settled, and what the session derives from it at once.
struct Agreed {
    n: handshake::Negotiated,
    /// `Copy` so the control task's `async move` and SessionContext both keep it.
    codec: crate::encode::Codec,
    live_reconfig_ok: bool,
    format_change_ok: bool,
    /// The further screens the client asked for, when it speaks them.
    screens: Option<Vec<v2msg::Screen>>,
}

/// The handshake under [`HANDSHAKE_TIMEOUT`], then [`Phase::BringUp`](crate::lifecycle::Phase).
async fn negotiate_session(
    scope: &mut SessionScope,
    (send, recv): (&mut link::CtlSend, &mut link::CtlReader),
    first: &ClientHello,
    host: &SessionHost,
    who: &Admitted,
    resolved: crate::profiles::Resolved,
) -> Result<Agreed> {
    let prep_done = scope.prep_done();
    let life = scope.life();
    let negotiation = handshake::Negotiation {
        conn: scope.conn(),
        first,
        source: host.opts.source,
        frames: host.opts.frames,
        bringup: &who.bringup,
        quit: life.quit_flag(),
        stop: life.stop_flag(),
        prep_done,
        grants: who.grants,
        expires_in_secs: who.expires_in_secs,
        resolved,
        knobs: who.knobs,
    };
    let n = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        handshake::negotiate(negotiation, send, recv),
    )
    .await
    .map_err(|_| anyhow!("handshake timed out after {HANDSHAKE_TIMEOUT:?}"))??;
    life.enter(crate::lifecycle::Phase::BringUp);
    tracing::info!(
        client_kind = n.client_link.kind,
        client_mbps = n.client_link.mbps,
        host_kind = n.host_link.iface_kind,
        host_mbps = n.host_link.link_mbps,
        "link facts of both ends"
    );
    let join_live = n.joined.is_some();
    // Live reconfigure is off for gamescope (resize must not relaunch the title),
    // `identity: per-client-mode` (resize would resolve a different slot), a monitor
    // mirror (physical head ignores the requested mode) and a `join` session (the mode
    // is the owner's). Synthetic stays on. Captured once here, the mirror pin too: a console
    // change mid-session must not change this session's answer.
    let mirrored = crate::session_plan::mirrored();
    let live_reconfig_ok = {
        let per_client_mode_identity = crate::vdisplay::policy::prefs()
            .configured_effective()
            .is_some_and(|e| e.identity == crate::vdisplay::policy::Identity::PerClientMode);
        reconfig_allowed(
            n.compositor,
            per_client_mode_identity,
            mirrored || join_live,
        )
    };
    // A mirror and a `join` session share another session's capture: their format stays.
    let format_change_ok = !(mirrored || join_live);
    let codec = crate::encode::codec_from_wire(n.welcome.codec);
    tracing::info!(
        peer = %scope.conn().remote_address(),
        mode = ?n.hello.mode,
        compositor = n.compositor.map(|c| c.id()).unwrap_or("none"),
        gamepad = n.welcome.gamepad.as_str(),
        // Build + the shell that dialled, so two sessions from one device are told apart here.
        // "-" is a client too old to send one.
        client = n.client_label.as_deref().unwrap_or("-"),
        "handshake complete — streaming"
    );
    let screens = n
        .features
        .has(punktfunk_core::quic::v2::registry::FEATURE_SCREENS)
        .then(|| first.screens.clone());
    Ok(Agreed {
        n,
        codec,
        live_reconfig_ok,
        format_change_ok,
        screens,
    })
}

/// What the launch and the stream take of the session's lanes.
struct Wired {
    stream_ends: wiring::StreamEnds,
    shared: wiring::SessionShared,
    controls: crate::session_status::SessionControls,
    launch_outcome_tx: crate::gamelease::OutcomeTx,
    /// Filled by the stream thread's encoder open; the input thread reads it.
    frame_map: input::FrameMap,
    /// Filled by the stream thread once it has a gamescope; the clipboard reads it.
    #[cfg(target_os = "linux")]
    gamescope_xwayland: pf_clipboard::GamescopeXwayland,
    client_name: Option<String>,
    /// Stream loop parks the seat pointer through the same path client input takes.
    #[cfg(target_os = "linux")]
    input_tx: std::sync::mpsc::SyncSender<ClientInput>,
}

/// The session's lanes, the path-MTU watch, the clipboard, the control task and the access
/// watch. The lanes' other ends go to the planes and the stream.
async fn wire_session(
    scope: &mut SessionScope,
    (ctrl_send, ctrl_recv): (link::CtlSend, link::CtlReader),
    host: &SessionHost,
    who: &mut Admitted,
    agreed: &Agreed,
    // The native endpoint's own socket carries the video, not a browser's datagrams.
    shared_plane: bool,
) -> (Wired, PlaneEnds) {
    let (conn, n) = (scope.conn(), &agreed.n);
    // Handshake stream stays open: Reconfigure → data plane rebuilds capture/encoder;
    // ProbeRequest → FLAG_PROBE burst. Inbound and outbound multiplexed with `select!`.
    let SessionWiring {
        control: control_ends,
        stream: stream_ends,
        shard,
        shared,
    } = SessionWiring::new(
        &n.welcome,
        host.opts.source,
        crate::send_pacing::Ports::of(
            (n.host_link.iface_kind, n.host_link.link_mbps),
            (n.client_link.kind, n.client_link.mbps),
        ),
    );
    // Shard renegotiation only if `Hello::max_shard_payload` and not PyroWave (PyroWave pins
    // the Welcome value for the session; a mid-stream re-key would desync). Not for a browser
    // either: the driver's targets are UDP-over-IP maths, and a WebTransport datagram also
    // carries QUIC and HTTP/3 framing, so a grow it computed would not fit.
    let shard_reneg = (n.hello.max_shard_payload > 0
        && agreed.codec != crate::encode::Codec::PyroWave
        && shared_plane)
        .then_some(wire_mtu::ShardReneg {
            change_tx: shard.change_tx,
            ack_rx: shard.ack_rx,
            apply_tx: shard.apply_tx,
        });
    // Path-MTU watch for the connection's life: clamp for the next session, and heal/grow
    // this one if the driver exists, including after the client moves.
    wire_mtu::spawn_watch(
        conn.clone(),
        n.welcome.shard_payload as usize,
        n.hello.max_shard_payload,
        shard_reneg,
    );
    let gamescope_xwayland = pf_clipboard::GamescopeXwayland::default();
    // Control task flips on `ClipControl`; lifecycle clears it if CLIPBOARD is revoked.
    let clip_enabled = Arc::new(AtomicBool::new(false));
    let clip = start_clipboard(
        conn,
        who.grants,
        clip_enabled.clone(),
        clip_target(n.compositor, &gamescope_xwayland),
    )
    .await;
    let clip_available = clip.available;
    let Lanes {
        access: (access_tx, access_rx),
        audio_rx,
        pad_rx,
        launch_outcome: (launch_outcome_tx, launch_outcome_rx),
        feedback_rx,
        audio_config_rx,
        controls,
        planes,
    } = session_lanes(host, who, n);
    let to_control = ToControl {
        ctrl_send,
        ctrl_recv,
        ends: control_ends,
        shared: shared.clone(),
        input_tx: planes.input.0.clone(),
        feedback_rx,
        feedback_tx: planes.feedback_tx.downgrade(),
        clip_enabled: clip_enabled.clone(),
        clip,
        session_grants: controls.grants.clone(),
        access_rx,
        audio_rx,
        audio_config_rx,
        pad_rx,
        launch_outcome_rx,
    };
    spawn_control(scope, host, who, agreed, to_control);
    let client_name = client_name(&host.np, who.fp_hex.as_deref(), &n.hello);
    watch_access(
        scope,
        who,
        &controls,
        &clip_enabled,
        access_tx,
        &client_name,
    );
    // No backend: decline fetches instead of hanging (coordinator owns `accept_bi` when live).
    if !clip_available && pf_clipboard::enabled() {
        if let Some(quic) = conn.as_quic() {
            pf_clipboard::spawn_decline_loop(quic.clone());
        }
    }
    let wired = Wired {
        stream_ends,
        shared,
        controls,
        launch_outcome_tx,
        frame_map: input::FrameMap::default(),
        #[cfg(target_os = "linux")]
        gamescope_xwayland,
        client_name,
        #[cfg(target_os = "linux")]
        input_tx: planes.input.0.clone(),
    };
    (wired, planes)
}

/// The session's lanes beside [`SessionWiring`]'s, both ends, and the controls its management
/// routes act on.
struct Lanes {
    access: (
        tokio::sync::mpsc::UnboundedSender<AccessUpdate>,
        tokio::sync::mpsc::UnboundedReceiver<AccessUpdate>,
    ),
    audio_rx: tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::AudioState>,
    pad_rx: tokio::sync::mpsc::UnboundedReceiver<input::PadToClient>,
    launch_outcome: (
        crate::gamelease::OutcomeTx,
        tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::LaunchOutcome>,
    ),
    feedback_rx: tokio::sync::mpsc::Receiver<punktfunk_core::quic::v2::dgram::Feedback>,
    audio_config_rx:
        tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::v2::msg::AudioConfig>,
    controls: crate::session_status::SessionControls,
    planes: PlaneEnds,
}

fn session_lanes(host: &SessionHost, who: &Admitted, n: &handshake::Negotiated) -> Lanes {
    // Lifecycle and the per-session management routes → control task. Both lanes stay
    // open for the whole session: the console can re-point or mute an anonymous client too.
    let (access_tx, access_rx) = tokio::sync::mpsc::unbounded_channel::<AccessUpdate>();
    let (audio_tx, audio_rx) =
        tokio::sync::mpsc::unbounded_channel::<punktfunk_core::quic::AudioState>();
    // Input thread → client: which player each of this session's pads is.
    let (pad_tx, pad_rx) = tokio::sync::mpsc::unbounded_channel::<input::PadToClient>();
    let pad_writes = n
        .features
        .has(punktfunk_core::quic::v2::registry::FEATURE_PAD_WRITES);
    // The device's stored player pick. Keyed by the pairing fingerprint, never by
    // an address: the same device reconnecting is the same player.
    let fp_hex = who.fp_hex.as_deref();
    let preferred_pad_slot = fp_hex.and_then(|fp| host.np.pad_slot_of(fp));
    let pad_id = crate::inject::pad_pool::PadIdentity::new(fp_hex, preferred_pad_slot);
    // Launch verdict lane. Unbounded and opened here so the launch's library resolve can
    // refuse onto it before the stream thread exists.
    let launch_outcome = tokio::sync::mpsc::unbounded_channel();
    // What `DELETE /session/{id}` and its siblings act on. `ceiling` is the pairing's own
    // mask: a live re-point clamps to it, so the console never grants past the pairing.
    let controls = crate::session_status::SessionControls {
        // One relaxed load per event; the lifecycle task is the only writer after admission.
        grants: Arc::new(AtomicU32::new(who.grants)),
        ceiling: Arc::new(AtomicU32::new(who.grants)),
        muted: Arc::new(AtomicBool::new(false)),
        deadline_unix: Arc::new(std::sync::atomic::AtomicI64::new(
            who.deadline_unix.unwrap_or(0),
        )),
        access_tx: Some(access_tx.clone()),
        audio_tx: Some(audio_tx),
        pad_slots: Arc::new(std::sync::atomic::AtomicU16::new(0)),
        fingerprint: who.fp_hex.clone(),
        preset: n.preset.clone(),
        profile: Some(who.profile_ref.clone()),
        pad_owner: pad_id.owner,
        preferred_pad_slot: Arc::new(std::sync::atomic::AtomicU8::new(
            preferred_pad_slot.unwrap_or(crate::session_status::NO_PAD_SLOT),
        )),
        // Written by the input thread, read by `GET /session/{id}/pads`.
        pads: Arc::new(crate::pad_feed::PadFeed::new()),
    };
    // One bounded channel for pointer/keyboard and rich input, fed by the datagram loop and
    // by the control loop's key edges. Unbounded is RSS DoS: the producer outruns the
    // consumer; pen batches amplify. Drop is correct — stale input is already worthless;
    // the injector re-syncs from the next event.
    const INPUT_QUEUE_DEPTH: usize = 1024;
    let input = std::sync::mpsc::sync_channel::<ClientInput>(INPUT_QUEUE_DEPTH);
    // The client's feedback datagrams, datagram reader → control task. Bounded: the peer sets
    // the rate, and each datagram repeats the last closed window, so a full lane drops.
    const FEEDBACK_DEPTH: usize = 64;
    let (feedback_tx, feedback_rx) = tokio::sync::mpsc::channel(FEEDBACK_DEPTH);
    // The audio thread's `AudioConfig`s, onto the control stream. Only a client that reopens
    // its player gets one.
    let (audio_config_tx, audio_config_rx) = tokio::sync::mpsc::unbounded_channel();
    let audio_epochs = n
        .features
        .has(punktfunk_core::quic::v2::registry::FEATURE_AUDIO_EPOCH)
        .then_some(audio_config_tx);
    Lanes {
        access: (access_tx, access_rx),
        audio_rx,
        pad_rx,
        launch_outcome,
        feedback_rx,
        audio_config_rx,
        controls,
        planes: PlaneEnds {
            input,
            pad_id,
            pad_tx,
            pad_writes,
            feedback_tx,
            audio_epochs,
        },
    }
}

/// The control task's ends of the session's lanes, named as [`control::Task`] names them.
struct ToControl {
    ctrl_send: link::CtlSend,
    ctrl_recv: link::CtlReader,
    ends: wiring::ControlEnds,
    shared: wiring::SessionShared,
    input_tx: std::sync::mpsc::SyncSender<ClientInput>,
    feedback_rx: tokio::sync::mpsc::Receiver<punktfunk_core::quic::v2::dgram::Feedback>,
    feedback_tx: tokio::sync::mpsc::WeakSender<punktfunk_core::quic::v2::dgram::Feedback>,
    clip_enabled: Arc<AtomicBool>,
    clip: pf_clipboard::ClipCoord,
    session_grants: Arc<AtomicU32>,
    access_rx: tokio::sync::mpsc::UnboundedReceiver<AccessUpdate>,
    audio_rx: tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::AudioState>,
    audio_config_rx:
        tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::v2::msg::AudioConfig>,
    pad_rx: tokio::sync::mpsc::UnboundedReceiver<input::PadToClient>,
    launch_outcome_rx: tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::LaunchOutcome>,
}

/// The control task, over the handshake's control stream.
fn spawn_control(
    scope: &SessionScope,
    host: &SessionHost,
    who: &Admitted,
    agreed: &Agreed,
    to: ToControl,
) {
    let conn = scope.conn();
    let (n, codec, source, knobs) = (&agreed.n, agreed.codec, host.opts.source, who.knobs);
    let (hello, welcome, host_link, features) = (&n.hello, &n.welcome, &n.host_link, &n.features);
    let ToControl {
        ctrl_send,
        ctrl_recv,
        ends,
        shared,
        input_tx,
        feedback_rx,
        feedback_tx,
        clip_enabled,
        clip,
        session_grants,
        access_rx,
        audio_rx,
        audio_config_rx,
        pad_rx,
        launch_outcome_rx,
    } = to;
    // Only sources that can keep encoder and packetizer FEC in one wire budget adapt it.
    // Synthetic-abr derives frame bytes from FEC every frame; the virtual path publishes
    // a proposal only after its encoder accepts the matching rate. Fixed synthetic and
    // the standalone software source have no coordinated retarget path.
    let adaptive_fec = adaptive_fec_for(source, knobs.fec_pct.is_some());
    // Negotiated rate; PyroWave retarget-refusals ack this pin.
    let session_bitrate_kbps = welcome.bitrate_kbps;
    scope.task(control::run(control::Task {
        ctrl_send,
        ctrl_recv,
        clock: conn.v2_session().clock.clone(),
        input_tx,
        initial_mode: hello.mode,
        stream_config: v2msg::StreamConfig {
            stream: 0,
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
        live_reconfig_ok: agreed.live_reconfig_ok,
        // `0` is an older client's byte: it decodes HEVC.
        client_codecs: match hello.video_codecs {
            0 => punktfunk_core::quic::CODEC_HEVC,
            set => set,
        },
        preferred_codec: hello.preferred_codec,
        // The synthetic source encodes test frames under any codec it is told; the others switch
        // within what the session's resolution emits.
        host_codecs: if source == Punktfunk1Source::Synthetic {
            punktfunk_core::quic::CODEC_H264
                | punktfunk_core::quic::CODEC_HEVC
                | punktfunk_core::quic::CODEC_AV1
        } else {
            n.encoder.wire_caps()
        },
        format_change_ok: agreed.format_change_ok
            && features.has(punktfunk_core::quic::v2::registry::FEATURE_FORMAT_EPOCH),
        adaptive_fec,
        session_bitrate_kbps,
        // Automatic, so negotiable: the governor may move it. PyroWave's floor still binds.
        bitrate_automatic: hello.bitrate_kbps == 0,
        pyrowave_automatic: hello.bitrate_kbps == 0 && codec == crate::encode::Codec::PyroWave,
        pyrowave_bpp: n.pyrowave_bpp,
        pyrowave_max_kbps: knobs.pyrowave_max_kbps,
        wire_bytes: u64::from(welcome.shard_payload)
            + punktfunk_core::abr::budget::SHARD_WIRE_OVERHEAD,
        audio_kbps: audio_reserved_kbps(welcome),
        ack_reason: n.abr_features & punktfunk_core::quic::EXT_ABR_ACK_REASON != 0,
        feedback_rx,
        feedback_tx,
        probe_only: n.probe_only,
        ends,
        shared,
        clip_enabled,
        clip,
        session_grants,
        access_rx,
        audio_rx,
        audio_config_rx,
        pad_rx,
        launch_outcome_rx,
        peer: conn.remote_address().ip(),
        plane: conn.plane(),
        counters: who.counters.clone(),
        stats: host.stats.clone(),
    }));
}

/// The access task: the expiry clock and the trust record's watch. Only a fingerprint has a
/// record to watch; with no record there is nothing to expire.
fn watch_access(
    scope: &SessionScope,
    who: &mut Admitted,
    controls: &crate::session_status::SessionControls,
    clip_enabled: &Arc<AtomicBool>,
    access_tx: tokio::sync::mpsc::UnboundedSender<AccessUpdate>,
    client_name: &Option<String>,
) {
    match (who.fp_hex.clone(), who.access_watch.take()) {
        (Some(fp_hex), Some(watch_rx)) => {
            let device = crate::events::DeviceRef {
                name: client_name
                    .clone()
                    .unwrap_or_else(|| crate::native_pairing::sanitize_device_name("", &fp_hex)),
                fingerprint: fp_hex,
                plane: crate::events::Plane::Native,
            };
            scope.task(access_lifecycle(
                scope.conn().clone(),
                watch_rx,
                controls.clone(),
                clip_enabled.clone(),
                access_tx,
                device,
                scope.life().clone(),
            ));
        }
        _ => drop(access_tx),
    }
}

/// The data plane bound, then the source on its stream thread until it returns, or until
/// [`STREAM_STOP_GRACE`] after `stop`.
async fn stream_session(
    scope: &mut SessionScope,
    host: &SessionHost,
    who: Admitted,
    agreed: Agreed,
    wired: Wired,
    launch: Launch,
    data_plane: DataPlane,
) -> Result<()> {
    let (conn, codec) = (scope.conn().clone(), agreed.codec);
    let Agreed { mut n, screens, .. } = agreed;
    let Wired {
        stream_ends,
        shared,
        controls,
        launch_outcome_tx,
        frame_map,
        #[cfg(target_os = "linux")]
        gamescope_xwayland,
        client_name,
        #[cfg(target_os = "linux")]
        input_tx,
    } = wired;
    let (hello, welcome) = (&n.hello, &n.welcome);
    let clear = n
        .features
        .has(punktfunk_core::quic::v2::registry::FEATURE_CLEAR_PLACEMENT);
    let (transport, wire_sock, media) = bind_data_plane(
        data_plane,
        &who.bringup,
        clear,
        shared.offload_switch.clone(),
    )?;
    let session = Session::new(welcome.session_config(Role::Host), media, transport)
        .map_err(|e| anyhow!("host session: {e:?}"))?;
    let common = StreamCommon {
        session,
        mode: hello.mode,
        seconds: host.opts.seconds,
        life: scope.life().clone(),
        counters: who.counters,
        ends: stream_ends,
        shared,
        bitrate_kbps: welcome.bitrate_kbps,
        audio_reserved_kbps: audio_reserved_kbps(welcome),
        shard_payload: welcome.shard_payload,
        // 0xCF host-timing only if the client advertised the cap.
        timing_conn: (hello.video_caps & punktfunk_core::quic::VIDEO_CAP_HOST_TIMING != 0)
            .then(|| conn.clone()),
        // Client reassembles probe filler in its own index window. Clear → decline probes.
        probe_seq: hello.video_caps & punktfunk_core::quic::VIDEO_CAP_PROBE_SEQ != 0,
        stats: host.stats.clone(),
        client_label: launch.client_label.clone(),
        bringup: who.bringup,
        wire_sock,
        codec,
        controls,
        client_name,
        // From Welcome colour, not from depth: a 10-bit SDR session is 10 + SDR.
        hdr: welcome.color.is_hdr(),
        bit_depth: welcome.bit_depth,
        color: welcome.color,
        // `Yuv444` only when the handshake gate passed.
        chroma: if welcome.chroma_format == punktfunk_core::quic::CHROMA_IDC_444 {
            crate::encode::ChromaFormat::Yuv444
        } else {
            crate::encode::ChromaFormat::Yuv420
        },
    };
    // Explicit-rate PyroWave: the client's ramp closes with one lower pin.
    let fit_pin = hello.bitrate_kbps != 0 && codec == crate::encode::Codec::PyroWave;
    let (source, frames, probe_only, prep) = (
        host.opts.source,
        host.opts.frames,
        n.probe_only,
        n.prep.take(),
    );
    let display = display_half(
        scope,
        host,
        who.knobs,
        n,
        screens,
        DisplayLanes {
            launch_outcome: launch_outcome_tx,
            frame_map,
            #[cfg(target_os = "linux")]
            gamescope_xwayland,
            #[cfg(target_os = "linux")]
            input_tx,
        },
        launch,
    );
    // Client address: what the registry groups sessions of one NAT or tunnel by.
    let (plane, peer) = (conn.plane(), conn.remote_address().ip());
    // Native thread: no async on the hot path.
    let stream_thread = tokio::task::spawn_blocking(move || {
        run_source(
            common, source, frames, probe_only, fit_pin, plane, peer, display, prep,
        )
    });
    // `stop` is advisory: a stuck syscall inside an iteration never sees it, and teardown
    // waits on this join. Bound the wait: after `STREAM_STOP_GRACE`, abandon the thread
    // (cannot cancel a blocking thread) so the slot and admission entry come back.
    tokio::select! {
        joined = stream_thread => joined.context("stream thread")??,
        () = stop_overdue(scope.life().stop_flag()) => {
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

/// What the display half takes of the session's lanes.
struct DisplayLanes {
    launch_outcome: crate::gamelease::OutcomeTx,
    frame_map: input::FrameMap,
    #[cfg(target_os = "linux")]
    gamescope_xwayland: pf_clipboard::GamescopeXwayland,
    #[cfg(target_os = "linux")]
    input_tx: std::sync::mpsc::SyncSender<ClientInput>,
}

/// The virtual source's display half. The stream thread owns it for every source, so the
/// launch claim and the input sender drop with the thread, before teardown joins input.
fn display_half(
    scope: &SessionScope,
    host: &SessionHost,
    knobs: SessionKnobs,
    n: handshake::Negotiated,
    screens: Option<Vec<v2msg::Screen>>,
    lanes: DisplayLanes,
    launch: Launch,
) -> impl FnOnce(StreamCommon) -> SessionContext + use<> {
    let conn = scope.conn().clone();
    // Re-points input across compositor switches and hands identity to backends.
    #[cfg(target_os = "linux")]
    let (isolation, input_route, inj_session_tx, follows_seat) = (
        scope.planes().isolation.clone(),
        scope.planes().input_route.clone(),
        scope.planes().inj_session_tx.clone(),
        plays_seat_mode(),
    );
    #[cfg(target_os = "linux")]
    let inj_tx = host.inj_tx.clone();
    let host_knobs = host.knobs;
    let handshake::Negotiated {
        hello,
        welcome,
        compositor,
        gamescope_route,
        encoder,
        pyrowave_bpp,
        slices,
        joined,
        features,
        ..
    } = n;
    // Read back from what the ServerHello agreed, like `cursor_forward` below.
    let format_facts = handshake::FormatFacts::of(
        &hello,
        crate::session_plan::StudioAsk::of(&features),
        compositor,
        gamescope_route.as_ref(),
        &knobs,
    );
    let reframe_to = joined.as_ref().map(|(_, view)| {
        (
            punktfunk_core::video_fit::VideoFit::from_wire(hello.video_fit),
            *view,
        )
    });
    // Read back from Welcome, not recomputed (would re-probe and could drift).
    let cursor_forward = welcome.host_caps & punktfunk_core::quic::HOST_CAP_CURSOR != 0;
    // Mid-stream resize counterpart; latest accepted Reconfigure wins.
    let resize_ms: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    move |common| SessionContext {
        common,
        compositor: compositor
            .expect("the Virtual source resolves a compositor during the handshake"),
        gamescope_route,
        format_facts,
        encoder,
        host_knobs,
        knobs,
        bitrate_auto: hello.bitrate_kbps == 0,
        pyrowave_bpp,
        cursor_forward,
        // Sentinel-headed streamed blocks: ship early FEC while the AU tail still encodes.
        streamed_au: hello.video_caps & punktfunk_core::quic::VIDEO_CAP_STREAMED_AU != 0,
        slices,
        conn,
        launch: launch.command,
        launch_target: launch.target,
        launch_claim: launch.claim,
        launch_stamp: launch.stamp,
        launch_owner: launch.owner,
        launch_outcome: lanes.launch_outcome,
        // `None` = older client or no HDR: the EDID and 0xCE take built-in defaults.
        client_hdr: hello.display_hdr.map(crate::encode::hdr_meta_from_wire),
        join_live: joined.is_some(),
        reframe_to,
        frame_map: lanes.frame_map,
        #[cfg(target_os = "linux")]
        gamescope_xwayland: lanes.gamescope_xwayland,
        resize_ms,
        #[cfg(target_os = "linux")]
        input_tx: lanes.input_tx,
        #[cfg(target_os = "linux")]
        isolation,
        #[cfg(target_os = "linux")]
        input_route,
        #[cfg(target_os = "linux")]
        inj_shared_tx: inj_tx,
        #[cfg(target_os = "linux")]
        inj_session_tx,
        #[cfg(target_os = "linux")]
        follows_seat,
        screens,
    }
}

/// A seat profile plays on its seat's host: send the client there, or say why not. `Some` when
/// the session ended in a redirect; `None` when it plays here.
async fn place_profile(
    conn: &link::SessionLink,
    send: &mut link::CtlSend,
    fp: Option<String>,
    first: &ClientHello,
    resolved: &crate::profiles::Resolved,
) -> Result<Option<Served>> {
    use crate::seats::placement::{place, Asker, Placement};
    let follows = first
        .features
        .has(punktfunk_core::quic::v2::registry::FEATURE_PROFILES);
    let browser = conn.is_web();
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
            browser,
        };
        place(&who, &asker)
    })
    .await
    .context("placement task")?;
    match placed {
        Placement::Here => Ok(None),
        Placement::Redirect(to) => {
            tracing::info!(profile = %resolved.id, seat = %to.seat_name, port = to.port,
                "redirected to the profile's seat");
            redirect(conn, send, &to).await?;
            Ok(Some(Served::Session))
        }
        Placement::Refuse(reason) => {
            close_rejected(conn, reason).await;
            anyhow::bail!("seat refused: {reason}");
        }
    }
}

/// How often a redirected guest's seat is asked whether it still plays there.
const SEAT_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a redirected guest has to show up on its seat: it dials, then the seat brings the
/// picture up before it lists the session.
const SEAT_LANDING: std::time::Duration = std::time::Duration::from_secs(120);

/// How long the seats may stay silent before they no longer hold a guest: a seat host that
/// hangs is not serving anyone.
const SEAT_SILENCE: std::time::Duration = std::time::Duration::from_secs(60);

/// Holds each "this session" guest the startup sweep kept, as a redirect holds its own: the guest
/// plays on a seat of a box whose host restarted. Spawns tasks, so it runs inside the runtime.
pub(crate) fn hold_guests_kept_at_start(np: &Arc<NativePairing>) {
    for (fp, landed) in np.take_kept_at_start() {
        hold_while_on_seat(np.session_started(&fp), fp, landed);
    }
}

/// A guest on a "this session" record plays on a seat after the redirect ended the door's own
/// session. Holds the count until it has played there and left, so the record outlasts the
/// play and not the redirect. The grace after the last count is the usual one. `landed` is true
/// for a guest already seen on a seat.
fn hold_while_on_seat(
    session: crate::native_pairing::SessionCountGuard,
    fp: String,
    mut landed: bool,
) {
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut heard = started;
        loop {
            tokio::time::sleep(SEAT_POLL).await;
            let fp = fp.clone();
            let asked = tokio::task::spawn_blocking(move || crate::seats::plays_on_a_seat(&fp));
            let here = tokio::time::timeout(SEAT_SILENCE, asked)
                .await
                .ok()
                .and_then(Result::ok)
                .flatten();
            if let Some(here) = here {
                heard = std::time::Instant::now();
                landed |= here;
            }
            if hold_over(here, landed, started.elapsed(), heard.elapsed()) {
                break;
            }
        }
        drop(session);
    });
}

/// A redirected guest's hold ends once it is off the seat after playing there, or never came,
/// or no seat has said anything for [`SEAT_SILENCE`]. `here` is `None` when a seat can't say.
fn hold_over(
    here: Option<bool>,
    landed: bool,
    waited: std::time::Duration,
    silent: std::time::Duration,
) -> bool {
    match here {
        Some(here) => !here && (landed || waited > SEAT_LANDING),
        None => silent > SEAT_SILENCE,
    }
}

/// The stream thread's body. A probe-only session answers probes and builds nothing; otherwise
/// `source` streams on `common`. The virtual source wraps `common` with `display`, and a display
/// prep that started at Welcome (`prep`) takes that context over and streams it.
#[allow(clippy::too_many_arguments)]
fn run_source(
    mut common: StreamCommon,
    source: Punktfunk1Source,
    frames: u32,
    probe_only: bool,
    fit_pin: bool,
    plane: crate::events::Plane,
    peer: std::net::IpAddr,
    display: impl FnOnce(StreamCommon) -> SessionContext,
    prep: Option<stream::PrepHandle>,
) -> Result<()> {
    // A display prep that started at Welcome (Windows) goes unreceived here and
    // aborts into keep-alive: the tag arrives on Start, after the prep began.
    if probe_only {
        let probe_seq = common.probe_seq;
        return stream::probe_only_stream(&mut common, probe_seq);
    }
    match source {
        Punktfunk1Source::Software => software_stream(
            &mut common.session,
            common.codec,
            common.mode,
            common.bitrate_kbps,
            common.life.stop_flag(),
            &common.ends.probe_rx,
            &common.ends.probe_result_tx,
            &common.shared.fec_target,
            common.probe_seq,
        ),
        Punktfunk1Source::Synthetic => {
            let opened_as = common.opened_as();
            synthetic_stream(
                &mut common.session,
                frames,
                common.life.stop_flag(),
                &common.ends,
                &common.shared.fec_target,
                common.timing_conn.as_ref(),
                common.probe_seq,
                opened_as,
            )
        }
        Punktfunk1Source::SyntheticAbr(shape) => synthetic_abr_stream(SynthAbrContext {
            common,
            content: shape.content,
            recovery: shape.recovery,
            answer: shape.answer,
            idr_pct: shape.idr_pct,
            bringup_delay: shape.bringup,
            fit_pin,
            plane,
            peer,
        }),
        Punktfunk1Source::Virtual => {
            let ctx = display(common);
            match prep {
                // Display prep started at Welcome: hand it the bound plane's context.
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

/// End the session when its connection closes. The first reason wins, so the host's own
/// close (game exit, clean finish) beats the `Lost` this would read it back as.
fn spawn_end_watch(scope: &SessionScope) {
    let (conn, life) = (scope.conn().clone(), scope.life().clone());
    scope.task(async move {
        let reason = conn.closed().await;
        // The client's own rule: anything that is not its quit code is the link going away.
        // A close this host made reads as `Lost` here too, which is why the paths that make
        // one end the session before they call it.
        life.end(if reason.closed_with(QUIT_CODE) {
            crate::events::SessionEndReason::Local
        } else {
            crate::events::SessionEndReason::Lost
        });
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

/// `ClientConnected` now, and `ClientDisconnected` with its reason once the connection closes.
fn emit_connected(scope: &SessionScope, client: crate::events::ClientRef) {
    crate::events::emit(crate::events::EventKind::ClientConnected {
        client: client.clone(),
    });
    let conn = scope.conn().clone();
    scope.task(async move {
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

/// A seat's own host, or the owner's on the owner's row in its headless session: every session
/// it serves plays the seat's desktop or Game Mode on shared planes and follows a switch. The
/// owner's host at the desk plays as the box's.
#[cfg(target_os = "linux")]
pub(crate) fn plays_seat_mode() -> bool {
    pf_paths::seat::role().runs_seat_session()
}

/// A seat profile whose bare connect opens Big Picture in its own gamescope.
pub(crate) fn opens_big_picture(profile: &crate::profiles::Resolved) -> bool {
    matches!(profile.os_account, crate::profiles::OsAccount::Seat { .. })
        && profile.home == crate::profiles::Home::Bigpicture
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
/// `clear_placement`: the client agreed to read sealed packets by their clear header.
fn bind_data_plane(
    data_plane: DataPlane,
    bringup: &crate::bringup::Trace,
    clear_placement: bool,
    offload: Arc<AtomicBool>,
) -> Result<BoundPlane> {
    bringup.mark("plane_bound");
    match data_plane {
        DataPlane::Web(plane) => {
            let v2 = plane.v2().clone();
            let media = punktfunk_core::session::MediaV2 {
                clock_origin_ns: v2.clock.origin_ns(),
                keys: None,
                clock: Some(v2.clock.clone()),
                clear_placement: false,
            };
            Ok((Box::new(plane), None, media))
        }
        DataPlane::Shared(v2) => {
            let suite = v2
                .suite()
                .ok_or_else(|| anyhow!("punktfunk/2 session reached media with no suite"))?;
            let keys = endpoint::media_keys(&v2.conn, &v2.session_id, suite)
                .ok_or_else(|| anyhow!("punktfunk/2 media keys: exporter refused"))?;
            let mut sender = punktfunk_core::transport::shared::MediaSender::new(
                &v2.media_socket,
                v2.conn.clone(),
            )
            .context("punktfunk/2 media sender")?;
            sender.set_offload_switch(offload);
            let media = punktfunk_core::session::MediaV2 {
                clock_origin_ns: v2.clock.origin_ns(),
                keys: Some(keys),
                clock: Some(v2.clock.clone()),
                clear_placement,
            };
            Ok((Box::new(sender), v2.media_socket.try_clone().ok(), media))
        }
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

/// The seat a profile streams on: the head of its id. Short enough for a socket name, and
/// stable, so keep-alive hands a kept spawn back to the same profile.
#[cfg(target_os = "linux")]
fn seat_id(profile_id: &str) -> String {
    profile_id[..profile_id.len().min(8)].to_string()
}

/// Isolation ids of the sessions streaming now. A second session on one profile is
/// `<seat>-2`: planes of its own, and no claim on the first one's kept display.
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

/// The isolated planes `id` streams on. `id` is also the registry's reuse key.
#[cfg(target_os = "linux")]
fn session_isolation(id: &str) -> crate::vdisplay::SessionIsolation {
    // Monitor-mode has no per-session sink — audio stays shared; input/mic still isolate.
    let sink =
        crate::audio::per_session_sink_possible().then(|| format!("punktfunk-speaker-iso-{id}"));
    crate::vdisplay::SessionIsolation::new(
        id.to_string(),
        sink,
        Some(format!("punktfunk-mic-{id}")),
    )
}

#[cfg(test)]
mod tests;
