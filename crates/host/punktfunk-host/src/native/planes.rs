//! A session's input and audio planes: the routes it injects and captures through, the side
//! threads it spawns on them, and the guards it holds while it streams. The scope holds the
//! planes and joins the threads.

use super::input::{self, input_thread, ClientInput};
use super::{audio::audio_thread, handshake, AudioCapSlot, Punktfunk1Source, SessionKnobs};
use super::{emit_connected, send_hdr_baseline, send_test_feedback};
#[cfg(target_os = "linux")]
use super::{plays_seat_mode, seat_id, session_isolation, SeatClaim};
use super::{Admitted, Agreed, SessionHost, SessionScope, Welcome, Wired};
use anyhow::{Context, Result};
use punktfunk_core::input::InputEvent;
use std::sync::Arc;

/// This session's input and mic planes. An isolated gamescope session
/// (`compositor_route::session_is_isolated`) gets its own; everyone else shares the
/// host-lifetime ones. Dropping it closes the pinned EIS connection and the isolated mic.
pub(super) struct SessionPlanes {
    /// Linux only. Identity is the device-fingerprint prefix, so keep-alive hands a kept spawn
    /// back to the same client.
    pub(super) isolation: Option<crate::vdisplay::SessionIsolation>,
    /// Pointer and keyboard: the pinned injector, else the host-lifetime one. Swappable.
    pub(super) input_route: input::InputRoute,
    #[cfg(target_os = "linux")]
    pub(super) inj_session_tx: Option<std::sync::mpsc::Sender<InputEvent>>,
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
    pub(super) fn mint(
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
                // A seat's own session stays on shared planes: a switch moves it to the desktop.
                None => compositor
                    .filter(|c| crate::compositor_route::session_is_isolated(*c, route))
                    .filter(|_| !plays_seat_mode())
                    .map(|_| {
                        // The profile's first session takes its id, a concurrent second one
                        // `<seat>-2`.
                        let claim = SeatClaim::take(&seat_id(&profile.id));
                        let iso = session_isolation(&claim.0);
                        tracing::info!(id = %claim.0, profile = %profile.id,
                            sink = iso.sink.as_deref().unwrap_or("-"),
                            "isolated gamescope session — per-session input/audio/mic planes");
                        seat = Some(claim);
                        iso
                    }),
            };
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
                input_route: input::InputRoute::new(inj_tx.clone()),
                mic_tx,
            }
        }
    }
}

/// The input thread, and the datagram reader that feeds it client input, mic frames and the
/// client's feedback. Grants, pad slots and the pad feed come from the session's `controls`;
/// the pointer route, seat devices and mic from the scope's planes; the rumble lease and the
/// Steam pad fold from its `knobs`. The scope joins the thread.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_input_plane(
    scope: &mut SessionScope,
    welcome: &Welcome,
    controls: &crate::session_status::SessionControls,
    counters: &Arc<crate::session_status::SessionCounters>,
    frame_map: &input::FrameMap,
    (input_tx, input_rx): (
        std::sync::mpsc::SyncSender<ClientInput>,
        std::sync::mpsc::Receiver<ClientInput>,
    ),
    pad_id: crate::inject::pad_pool::PadIdentity,
    pad_tx: tokio::sync::mpsc::UnboundedSender<input::PadToClient>,
    pad_writes: bool,
    feedback_tx: tokio::sync::mpsc::Sender<punktfunk_core::quic::v2::dgram::Feedback>,
    knobs: SessionKnobs,
) -> Result<()> {
    let conn = scope.conn().clone();
    let mic_tx = scope.planes().mic_tx.clone();
    {
        let conn = conn.clone();
        let stop = scope.life().stop_flag().clone();
        let gamepad = welcome.gamepad;
        // Read HOST_CAP_PAD_AUDIO back off Welcome so the input thread cannot disagree.
        let pad_audio_on = welcome.host_caps & punktfunk_core::quic::HOST_CAP_PAD_AUDIO != 0;
        let grants = controls.grants.clone();
        let pad_slots = controls.pad_slots.clone();
        let frame_map = frame_map.clone();
        let pad_feed = controls.pads.clone();
        let counters = counters.clone();
        let input_route = scope.planes().input_route.clone();
        scope
            .thread("punktfunk1-input", move || {
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
                    stop,
                    counters,
                    knobs,
                )
            })
            .context("spawn input thread")?;
    }
    input::spawn_datagram_reader(
        conn,
        controls.grants.clone(),
        counters.clone(),
        mic_tx,
        input_tx,
        feedback_tx,
    );
    Ok(())
}

/// The audio thread, except for the two frame-arithmetic sources: their clients want nothing
/// else on the wire, and the rig's budget carries the audio reservation without a capture
/// behind it. A joiner (`joined`) taps the owner's sink; `published` is where this session's
/// thread names the sink it captures. Best-effort: a failed spawn is logged and the session
/// streams without audio, because the other threads are already up.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_audio_plane(
    scope: &mut SessionScope,
    source: Punktfunk1Source,
    audio_cap: &AudioCapSlot,
    welcome: &Welcome,
    joined: Option<&crate::vdisplay::admission::LiveDisplay>,
    published: &Arc<std::sync::Mutex<Option<String>>>,
    controls: &crate::session_status::SessionControls,
    counters: &Arc<crate::session_status::SessionCounters>,
    audio_epochs: Option<
        tokio::sync::mpsc::UnboundedSender<punktfunk_core::quic::v2::msg::AudioConfig>,
    >,
) {
    if matches!(
        source,
        Punktfunk1Source::Synthetic | Punktfunk1Source::SyntheticAbr(..)
    ) {
        return;
    }
    let conn = scope.conn().clone();
    let stop = scope.life().stop_flag().clone();
    let cap = audio_cap.clone();
    let channels = welcome.audio_channels;
    // Format from Welcome bytes, not a second evaluation of the gate (config + live property).
    let audio_plane = handshake::AudioPlane::from_welcome(welcome);
    // Read the granted bit back off Welcome, then re-derive the same budget rung from it.
    let budget = handshake::audio_budget(
        welcome.host_caps & punktfunk_core::quic::HOST_CAP_AUDIO_RED != 0,
        welcome.bitrate_kbps,
        channels,
        audio_plane.layout,
    );
    // Isolated session captures its own named sink; `None` is the shared path. A joiner
    // taps the owner's sink either way: its isolated one, or the one the owner published.
    let iso_sink = scope.planes().isolation.clone().and_then(|i| i.sink);
    let join_live = joined.is_some();
    let tap_from = joined.map(|d| d.audio_sink.clone());
    let published = published.clone();
    let muted = controls.muted.clone();
    let counters = counters.clone();
    let spawned = scope.thread("punktfunk1-audio", move || {
        audio_thread(super::audio::PlaneArgs {
            conn,
            stop,
            audio_cap: cap,
            channels,
            budget,
            plane: audio_plane,
            sink: iso_sink,
            tap: join_live,
            published,
            tap_from,
            muted,
            counters,
            audio_epochs,
        })
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "audio thread spawn failed — session continues without audio");
    }
}

/// What the planes take of the session's lanes.
pub(super) struct PlaneEnds {
    pub(super) input: (
        std::sync::mpsc::SyncSender<ClientInput>,
        std::sync::mpsc::Receiver<ClientInput>,
    ),
    pub(super) pad_id: crate::inject::pad_pool::PadIdentity,
    pub(super) pad_tx: tokio::sync::mpsc::UnboundedSender<input::PadToClient>,
    pub(super) pad_writes: bool,
    pub(super) feedback_tx: tokio::sync::mpsc::Sender<punktfunk_core::quic::v2::dgram::Feedback>,
    pub(super) audio_epochs:
        Option<tokio::sync::mpsc::UnboundedSender<punktfunk_core::quic::v2::msg::AudioConfig>>,
}

/// The planes: the input and mic routes, the input thread and its datagram reader, the
/// admission entry, the audio thread, and the guards held while the session streams.
pub(super) fn spawn_planes(
    scope: &mut SessionScope,
    host: &SessionHost,
    who: &Admitted,
    agreed: &Agreed,
    wired: &Wired,
    ends: PlaneEnds,
) -> Result<()> {
    let (conn, n) = (scope.conn().clone(), &agreed.n);
    let (hello, welcome) = (&n.hello, &n.welcome);
    let joined = n.joined.as_ref().map(|(d, _)| d);
    // Claiming the default mic is a PipeWire round trip under a 5 s timeout, and this runtime
    // has two workers: off them, as the launch prep.
    scope.set_planes(tokio::task::block_in_place(|| {
        SessionPlanes::mint(
            joined,
            n.compositor,
            n.gamescope_route.as_ref(),
            &n.resolved,
            &host.inj_tx,
            host.mic_tx.clone(),
        )
    }));
    let PlaneEnds {
        input,
        pad_id,
        pad_tx,
        pad_writes,
        feedback_tx,
        audio_epochs,
    } = ends;
    let (controls, counters) = (&wired.controls, &who.counters);
    spawn_input_plane(
        scope,
        welcome,
        controls,
        counters,
        &wired.frame_map,
        input,
        pad_id,
        pad_tx,
        pad_writes,
        feedback_tx,
        who.knobs,
    )?;
    // Handshake complete: CONNECTED. A client rejected earlier never emits either.
    emit_connected(
        scope,
        crate::events::ClientRef {
            name: wired.client_name.clone().unwrap_or_default(),
            fingerprint: who.fp_hex.clone(),
            plane: conn.plane(),
            preset: n.preset.clone(),
            profile: Some(who.profile_ref.clone()),
        },
    );
    // The audio thread publishes the sink it captures here, for a joiner to tap.
    let audio_sink: Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let live = register_live(scope, n, &audio_sink);
    scope.hold(live);
    // `CLIENT_CAP_KEEP_HOST_AUDIO`: taken before the audio thread spawns, which opens
    // capture straight away and reads this to pick its topology.
    scope.hold(
        (hello.client_caps & punktfunk_core::quic::CLIENT_CAP_KEEP_HOST_AUDIO != 0)
            .then(crate::audio::capture_policy::keep_host_audio_guard),
    );
    let source = host.opts.source;
    spawn_audio_plane(
        scope,
        source,
        &host.audio_cap,
        welcome,
        joined,
        &audio_sink,
        controls,
        counters,
        audio_epochs,
    );
    if welcome.color.is_hdr() {
        send_hdr_baseline(&conn, hello.display_hdr);
    }
    if source == Punktfunk1Source::Synthetic && who.knobs.test_feedback {
        send_test_feedback(&conn);
    }
    // `$XDG_RUNTIME_DIR/punktfunk/stream` while this session streams. Retracts on every exit.
    scope.hold(crate::stream_marker::announce(
        crate::stream_marker::StreamInfo {
            width: hello.mode.width,
            height: hello.mode.height,
            refresh_hz: hello.mode.refresh_hz,
            hdr: welcome.color.is_hdr(),
            client: wired.client_name.clone().unwrap_or_default(),
            fingerprint: who.fp_hex.clone(),
            launch: hello.launch.clone(),
            plane: conn.plane(),
            preset: n.preset.clone(),
            profile: Some(who.profile_ref.clone()),
        },
    ));
    // Linux `PUNKTFUNK_PIN_CLOCKS`: refcounted vendor clock floor while any session streams.
    #[cfg(target_os = "linux")]
    scope.hold(crate::gpuclocks::session_pin());
    Ok(())
}

/// Mode-conflict admission: the handshake's reservation goes live at the Welcome's mode. Later
/// clients see this identity, mode and stop (and may `steal`), and a joiner finds the display,
/// its route and colour, the planes and the sink `audio_sink` names.
fn register_live(
    scope: &SessionScope,
    n: &super::handshake::Negotiated,
    audio_sink: &Arc<std::sync::Mutex<Option<String>>>,
) -> Option<crate::vdisplay::admission::LiveGuard> {
    let conn = scope.conn();
    let label = conn
        .peer_fingerprint()
        .map(|fp| hex::encode(&fp[..4]))
        .unwrap_or_else(|| "client".to_string());
    let mode = n.welcome.mode;
    let reservation = n
        .reservation
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()?;
    Some(reservation.register(
        (mode.width, mode.height, mode.refresh_hz),
        label,
        crate::vdisplay::admission::LiveDisplay {
            compositor: n.compositor,
            route: n.gamescope_route.clone(),
            isolation: scope.planes().isolation.clone(),
            audio_sink: audio_sink.clone(),
            hdr: n.welcome.color.is_hdr(),
            format: crate::vdisplay::admission::shares_capture(n.compositor).then_some(
                crate::vdisplay::admission::SampleFormat {
                    bit_depth: n.welcome.bit_depth,
                    chroma_444: n.welcome.chroma_format == punktfunk_core::quic::CHROMA_IDC_444,
                    full_range: n.welcome.color.full_range != 0,
                },
            ),
        },
        Some(conn.v2_session().session_id),
    ))
}
