//! Native `punktfunk/1` mid-stream control task.
//!
//! After handshake the control stream stays open. This task is its sole writer:
//! inbound client requests share a `select!` with outbound probe results, mode
//! corrections, bitrate retargets, pipeline gaps, shard-payload changes, cursor
//! shapes, clipboard offers, and access updates. Validated changes go to the
//! data-plane thread over the session's mpsc bridges.
//!
//! Every loss report, RFI and keyframe ask lands here, so the per-minute `link health` line
//! ([`crate::link_health`]) is counted and emitted here too.
//!
//! `select!` drops the inbound read future whenever a sibling fires, so framing
//! uses a [`FrameReader`](punktfunk_core::quic::v2::io::FrameReader). Optional channels whose sender can drop mid-session
//! (`clip_offer_rx`, `shard_change_rx`, `access_rx`) must disable their branch
//! on `None` or a closed mpsc busy-spins.
//!
//! Evidence: `design/shard-payload-reneg.md`, `design/per-client-access.md`,
//! `design/clipboard-and-file-transfer.md`.

use super::*;
use pf_clipboard::ClipCoordCmd;
use punktfunk_core::abr::governor::{ShareWindow, NO_SHARE_KBPS};
use punktfunk_core::quic::v2::{io as v2io, msg as v2msg};
use punktfunk_core::quic::{AckReason, ClipControl, ClipOffer, ClipState};

/// The ack this client can read. The reason byte goes only to a client that
/// asked for it (`EXT_ABR_ACK_REASON`); every other client gets the nine bytes
/// it has always got, whatever the host knows.
fn bitrate_ack(kbps: u32, why: AckReason, client_reads_reason: bool) -> BitrateChanged {
    BitrateChanged {
        bitrate_kbps: kbps,
        reason: client_reads_reason.then_some(why),
    }
}

/// When each of the governor's up-moves may next go out.
struct ShareClocks {
    room: std::time::Instant,
    lift: std::time::Instant,
}

impl ShareClocks {
    fn new(now: std::time::Instant) -> Self {
        ShareClocks {
            room: now + punktfunk_core::abr::governor::SHARE_CLOCK,
            lift: now + punktfunk_core::abr::governor::SHARE_LIFT_CLOCK,
        }
    }

    fn take(&mut self, now: std::time::Instant) -> punktfunk_core::abr::governor::Clocks {
        let out = punktfunk_core::abr::governor::Clocks {
            room: now >= self.room,
            lift: now >= self.lift,
        };
        if out.room {
            self.room = now + punktfunk_core::abr::governor::SHARE_CLOCK;
        }
        if out.lift {
            self.lift = now + punktfunk_core::abr::governor::SHARE_LIFT_CLOCK;
        }
        out
    }
}

/// What a client's report window leaves this session: the share of the path it is on, if
/// the governor has one to send.
///
/// The report is the boundary both figures are read over, so closing the window
/// and asking are one step ([`crate::session_status::share_for`]). A group of
/// one never has a share. The id is `0` until the video loop registers the
/// session, and before that there is nothing for a sibling to share with.
#[allow(clippy::too_many_arguments)]
fn delivery_share(
    now: std::time::Instant,
    packets_received: u64,
    counters: &crate::session_status::SessionCounters,
    window: &mut ShareWindow,
    clocks: &mut ShareClocks,
    automatic: bool,
    wire_bytes: u64,
) -> Option<u32> {
    let (offered, delivered, streaming) = window.close(
        now,
        counters.link.egress_bytes(),
        packets_received,
        wire_bytes,
    );
    counters
        .share
        .publish(now, automatic, offered, delivered, streaming);
    let id = counters.link.session_id();
    (id != 0)
        .then(|| crate::session_status::share_for(id, clocks.take(now)))
        .flatten()
}

/// Whether feedback number `n` is one this task has not acted on: ahead of `last` in wrap
/// order over `0..=max`. Moves `last` when it is. `0` carries nothing.
fn take_newer(n: u32, last: &mut u32, max: u32) -> bool {
    let ahead = n.wrapping_sub(*last) & max;
    if n == 0 || ahead == 0 || ahead > max / 2 {
        return false;
    }
    *last = n;
    true
}

/// Whether this probe request is as short as a bring-up ramp step.
///
/// The length bound is the ramp's exemption from the spacing. Without it a
/// client could hold the window open with 5 s bursts at the probe ceiling,
/// which is the uplink-pinning the spacing exists against.
fn is_ramp_length(req: &ProbeShaped) -> bool {
    req.duration_ms <= super::stream::RAMP_STEP_MAX_MS
}

/// One speed-test burst per 10 s. Each burst is already clamped (5 s, 10 Gbps);
/// without a count cap a client can pause video and pin the uplink.
///
/// A ramp step neither waits on the spacing nor starts it: a ramp cut short
/// by the first frame asks for its burst two seconds later, and that burst is
/// the only measurement the session gets. A short step just after the window
/// closed is still the ramp's — the client asks each step once the last one
/// drained, and a re-ask can cross the close.
#[derive(Default)]
struct ProbeSpacing {
    last: Option<std::time::Instant>,
    /// When a short step last arrived with the ramp window open.
    last_ramp_step: Option<std::time::Instant>,
}

impl ProbeSpacing {
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
    /// How long after the last in-window step a short step still counts as the
    /// ramp's. A step settles within a round trip of its answer; only in-window
    /// steps restart this clock, so it cannot be chained past the window.
    const RAMP_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

    /// Whether to serve this request. `short` is ramp length
    /// ([`is_ramp_length`]); `open` is the ramp window.
    fn admit(&mut self, now: std::time::Instant, short: bool, open: bool) -> bool {
        if short && open {
            self.last_ramp_step = Some(now);
            return true;
        }
        let in_grace = self
            .last_ramp_step
            .is_some_and(|t| now.duration_since(t) < Self::RAMP_GRACE);
        if short && in_grace {
            return true;
        }
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < Self::INTERVAL)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// A PyroWave session's pin against `SetBitrate` asks.
///
/// Every ask is refused with the pin — except one: an Automatic client's
/// bring-up ramp ends in a verdict ask, and while the ramp window is still
/// open a lower one becomes the pin. Never a raise, never twice, and a
/// refused ask does not spend the verdict.
struct PyroWavePin {
    /// The pin as it stands — what refused asks ack.
    kbps: u32,
    /// The client asked Automatic, so its ramp's verdict may fit the pin.
    automatic: bool,
    /// The verdict ask already landed.
    fit_taken: bool,
}

impl PyroWavePin {
    /// The rate to ack and send the encoder: the asked rate the one time the
    /// window admits it, the standing pin every other time. `ramp_open` is
    /// the bring-up window — it closes before the first frame.
    fn resolve(&mut self, asked_kbps: u32, ramp_open: bool) -> u32 {
        if self.automatic
            && !self.fit_taken
            && ramp_open
            && asked_kbps > 0
            && asked_kbps < self.kbps
        {
            self.fit_taken = true;
            self.kbps = asked_kbps;
        }
        self.kbps
    }
}

/// Named fields, not a 30-argument spawn: `retarget_rx` and `gap_rx` are both
/// bare `u32`, so a positional swap would compile and fail at runtime.
pub(super) struct Task {
    pub(super) ctrl_send: super::link::CtlSend,
    pub(super) ctrl_recv: super::link::CtlReader,
    /// The session's media clock: clock echoes leave in its time.
    pub(super) clock: std::sync::Arc<punktfunk_core::quic::v2::clock::SessionClock>,
    /// The input thread's queue, shared with the datagram loop: a key edge off the
    /// control stream lands in the same order-preserving line as the pointer.
    pub(super) input_tx: std::sync::mpsc::SyncSender<super::input::ClientInput>,
    pub(super) initial_mode: punktfunk_core::Mode,
    /// Epoch 0's `StreamConfig` with this host's link facts, sent as the task starts.
    pub(super) stream_config: v2msg::StreamConfig,
    /// The client moves its mode on configs (`FEATURE_STREAM_CONFIG`). Otherwise a switch is
    /// corrected with a second `Reconfigured`.
    pub(super) anchored: bool,
    pub(super) codec: crate::encode::Codec,
    pub(super) live_reconfig_ok: bool,
    pub(super) adaptive_fec: bool,
    pub(super) session_bitrate_kbps: u32,
    /// Automatic bitrate, so the shared-path governor may move this session. A
    /// client-set rate and a PyroWave pin are never touched.
    pub(super) bitrate_automatic: bool,
    /// PyroWave session whose client asked Automatic (`Hello.bitrate_kbps ==
    /// 0`): its bring-up ramp may lower the pin once while `ramp_open` holds.
    /// An explicit ask — resolved to the same pin regardless — and every other
    /// codec never get that window.
    pub(super) pyrowave_automatic: bool,
    /// One wire packet, bytes. Turns the client's delivery count into the rate
    /// the governor divides, and the shard the parity rule sizes against.
    pub(super) wire_bytes: u64,
    /// Audio reservation out of the wire budget. With [`Self::wire_bytes`] and
    /// the live mode it is the frame adaptive FEC has to protect.
    pub(super) audio_kbps: u32,
    /// Client set `EXT_ABR_ACK_REASON` in its `Start` block: its `BitrateChanged`
    /// may carry the reason byte. Clear for every shipped client, which rejects
    /// a longer ack, and for every client behind a host without `HOST_CAP2_EXT`.
    pub(super) ack_reason: bool,
    /// The client's feedback datagrams, from the session's datagram reader.
    pub(super) feedback_rx:
        tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::v2::dgram::Feedback>,
    /// A diagnostic session (`EXT_DELIVERY_PROBE_ONLY`): every probe is served, with no
    /// spacing. The session holds no pipeline, and the stream thread bounds what it costs.
    pub(super) probe_only: bool,
    /// The control halves of the session's channels to the stream thread.
    pub(super) ends: super::wiring::ControlEnds,
    /// Encoder truth read at `SetBitrate`, so the ack never exceeds what the encoder will run,
    /// and the FEC, link, ramp and cursor values this task shares with the stream thread.
    pub(super) shared: super::wiring::SessionShared,
    pub(super) clip_enabled: Arc<AtomicBool>,
    pub(super) clip: pf_clipboard::ClipCoord,
    /// LIVE grant mask, same atomic the datagram filter reads. Deadline/watch
    /// folds console edits in, so a later `ClipControl`/`ClipOffer` sees them.
    pub(super) session_grants: Arc<AtomicU32>,
    /// Sole writer for `AccessUpdate`s: the deadline/watch task and the per-session
    /// management route both send here, so both clear the clipboard the same way.
    pub(super) access_rx: tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::AccessUpdate>,
    /// Operator mute for this session (mgmt `PUT /session/{id}/audio`), so the client
    /// can name the silence rather than conceal a gap.
    pub(super) audio_rx: tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::AudioState>,
    /// OS pad slots this session holds, from the input thread. The client names the
    /// player it is from this; wire indices are per client and say nothing about it.
    /// And the feature reports Steam set on them, for the physical pads.
    pub(super) pad_rx: tokio::sync::mpsc::UnboundedReceiver<super::input::PadToClient>,
    /// What became of this session's library launch, from the launch site and
    /// from the lease when the game dies on the spot.
    pub(super) launch_outcome_rx:
        tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::LaunchOutcome>,
    /// Named on the per-minute `link health` line, so a journal sorts by client.
    pub(super) peer: std::net::IpAddr,
    /// Which plane carries this session; named on grant-drop warnings.
    pub(super) plane: crate::events::Plane,
    /// Shared block the encode and send threads bump; this task drains its link half.
    pub(super) counters: Arc<crate::session_status::SessionCounters>,
    /// Armed capture the per-minute line is also written into, so a bug report is one file.
    pub(super) stats: Arc<crate::stats_recorder::StatsRecorder>,
}

/// What the client hears about a delivered mode.
#[derive(Debug, PartialEq)]
enum Tell {
    /// The epoch's config, to a client that takes them.
    Config(v2msg::StreamConfig),
    /// A second `Reconfigured`, to a client that does not.
    Correct(Reconfigured),
}

/// A config for an epoch or mode the client does not hold yet, else a correction when the
/// client was told something else. `stream_config` is what the client holds.
fn tell(
    stream_config: &mut Option<v2msg::StreamConfig>,
    d: &super::wiring::Delivered,
) -> Option<Tell> {
    match stream_config {
        Some(cfg) if (cfg.epoch, cfg.mode) != (d.epoch, d.mode) => {
            (cfg.epoch, cfg.mode) = (d.epoch, d.mode);
            Some(Tell::Config(*cfg))
        }
        Some(_) => None,
        None => d.corrects.then_some(Tell::Correct(Reconfigured {
            accepted: true,
            mode: d.mode,
        })),
    }
}

/// Ends when the control stream closes or a data-plane channel drops.
pub(super) async fn run(task: Task) {
    let Task {
        mut ctrl_send,
        mut ctrl_recv,
        clock,
        input_tx,
        initial_mode,
        stream_config: initial_config,
        anchored,
        codec,
        live_reconfig_ok,
        adaptive_fec,
        session_bitrate_kbps,
        bitrate_automatic,
        pyrowave_automatic,
        wire_bytes,
        audio_kbps,
        ack_reason,
        mut feedback_rx,
        probe_only,
        ends:
            super::wiring::ControlEnds {
                reconfig_tx,
                keyframe_tx,
                rfi_tx,
                bitrate_tx,
                probe_tx,
                mut probe_result_rx,
                mut reconfig_result_rx,
                mut retarget_rx,
                mut gap_rx,
                mut shard_change_rx,
                shard_ack_tx,
                mut cursor_shape_rx,
            },
        shared:
            super::wiring::SessionShared {
                live_bitrate,
                encoder_ceiling,
                cadence_degraded,
                cadence_behind_score,
                client_packets_received,
                fec_target,
                fec_requested,
                link_kbps,
                delivery: _,
                ramp_open,
                cursor_client_draws,
            },
        clip_enabled,
        clip,
        session_grants,
        mut access_rx,
        mut audio_rx,
        mut pad_rx,
        mut launch_outcome_rx,
        peer,
        plane,
        counters,
        stats,
    } = task;
    let pf_clipboard::ClipCoord {
        available: clip_available,
        cmd_tx: clip_cmd_tx,
        offer_rx: mut clip_offer_rx,
    } = clip;
    // Once `clip_offer_rx` closes, disable its `select!` arm — a closed
    // mpsc is perpetually ready with `None`.
    let mut clip_offer_closed = false;
    // First-of-class `warn!` for grant drops. A revoked client spamming the
    // gated messages must not flood the log.
    let mut denied = crate::session_status::GrantDrops::new(plane);
    // Same closed-channel discipline as `clip_offer_closed`.
    let mut shard_change_closed = false;
    // `--open` anonymous sessions never spawn deadline/watch; the sender
    // drops immediately.
    let mut access_closed = false;
    // Same closed-channel discipline: the mute lane outlives nothing of its own.
    let mut audio_closed = false;
    // Pad slots end with the input thread.
    let mut pad_slots_closed = false;
    // Same again. The launch site drops its sender when the session ends.
    let mut launch_outcome_closed = false;
    let mut active = initial_mode;
    // PyroWave's pin, against the session's asks: an Automatic client's
    // bring-up ramp may lower it once while the window before the first
    // frame is open; every other ask is refused with the pin as it stands.
    let mut pyrowave_pin = PyroWavePin {
        kbps: session_bitrate_kbps,
        automatic: pyrowave_automatic,
        fit_taken: false,
    };
    // Backstop against Reconfigure spam. Data-plane drain-to-newest already
    // coalesces a resize drag; 500 ms is half the client's 1 s self-limit.
    const MIN_SWITCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    let mut last_accepted_switch: Option<std::time::Instant> = None;
    let mut probe_spacing = ProbeSpacing::default();
    // An RFI ask is a frame parity could not repair; the report that
    // closes the window carries only what parity did repair.
    let mut unrecovered = UnrecoveredRun::default();
    // The link's loss over a horizon one report window cannot see, which is
    // what sizes parity against the frame this session's budget buys.
    let mut fec_horizon = punktfunk_core::abr::budget::LossHorizon::default();
    // Shared-path governor: what this session offered and what reached it over
    // the last window, read at the same boundary so a shortfall describes one
    // stretch of link, plus the two clocks an up-move rides.
    let mut window = ShareWindow::new(std::time::Instant::now(), counters.link.egress_bytes());
    let mut share_clocks = ShareClocks::new(std::time::Instant::now());
    // One `link health` line a minute, ticking whether or not anything arrived: a reader must
    // be able to tell a clean minute from a host that stopped logging.
    let mut link = crate::link_health::LinkWindow::new(&counters.link);
    let mut link_tick = tokio::time::interval(std::time::Duration::from_secs(60));
    link_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` fires immediately; the first tick would close an empty window at 0 s.
    link_tick.tick().await;
    // Feedback numbers acted on so far; a repeat or a reordered older copy is skipped.
    let (mut last_window, mut last_ask) = (0u32, 0u32);
    // The client reads this host's link facts off the first config, anchored or not.
    if v2io::send(&mut ctrl_send, &initial_config).await.is_err() {
        return;
    }
    // What the client holds; `None` for a client that takes no configs.
    let mut stream_config = anchored.then_some(initial_config);
    loop {
        tokio::select! {
            fb = feedback_rx.recv() => {
                // The datagram reader ends with the connection.
                let Some(fb) = fb else { break };
                if fb.link_kbps != 0 && link_kbps.swap(fb.link_kbps, Ordering::Relaxed) != fb.link_kbps {
                    tracing::info!(link_kbps = fb.link_kbps, "client's link rate");
                }
                // The ask before the window: an RFI raised in a window counts in its report.
                if take_newer(u32::from(fb.ask), &mut last_ask, u16::MAX.into()) {
                    if fb.keyframe {
                        // Encode loop coalesces: a wedge fires several requests
                        // before the IDR lands.
                        tracing::debug!("client requested keyframe (decode recovery)");
                        link.note_keyframe_req();
                        if keyframe_tx.send(()).is_err() {
                            break;
                        }
                    } else if let Some((first, last)) = fb.invalidate {
                        // Encode loop falls back to a coalesced IDR when the range
                        // is too old or the encoder has no RFI.
                        tracing::debug!(
                            first,
                            last,
                            "client requested reference-frame invalidation (loss recovery)"
                        );
                        unrecovered.rfi();
                        link.note_rfi();
                        if rfi_tx.send((first, last)).is_err() {
                            break;
                        }
                    }
                }
                if take_newer(fb.window, &mut last_window, u32::MAX) {
                    let unrecovered_run = unrecovered.report(std::time::Instant::now());
                    link.note_loss(fb.loss_ppm, unrecovered_run);
                    link.sample_bands(
                        fec_target.load(Ordering::Relaxed),
                        live_bitrate.load(Ordering::Relaxed),
                    );
                    // The proposal lands on `fec_requested`; the stream loop
                    // publishes it to `fec_target` (what the send loop reads per
                    // frame) once the encoder accepts the matching rate.
                    // No-op when FEC is pinned (`PUNKTFUNK_FEC_PCT`).
                    if adaptive_fec {
                        let prev = fec_target.load(Ordering::Relaxed);
                        let target = punktfunk_core::abr::budget::fec_target(
                            fb.loss_ppm,
                            prev,
                            unrecovered_run,
                            punktfunk_core::abr::budget::FrameBudget {
                                budget_kbps: live_bitrate.load(Ordering::Relaxed),
                                audio_kbps,
                                shard_payload: wire_bytes
                                    .saturating_sub(
                                        punktfunk_core::abr::budget::SHARD_WIRE_OVERHEAD,
                                    )
                                    .try_into()
                                    .unwrap_or(u16::MAX),
                                fps: active.refresh_hz,
                            },
                            &mut fec_horizon,
                        );
                        fec_requested.store(target, Ordering::Release);
                        if prev != target {
                            tracing::debug!(
                                loss_ppm = fb.loss_ppm,
                                unrecovered_run,
                                fec_pct = target,
                                prev_fec_pct = prev,
                                "adaptive FEC adjusted"
                            );
                        }
                    }
                    // Unconditional: stall diagnosis needs `loss_ppm = 0` even
                    // when FEC is pinned or adaptive FEC is off. Saturate into
                    // the u32 bridge; the value only matters near zero.
                    client_packets_received.store(
                        fb.packets_received.min(u32::MAX as u64 - 1) as u32,
                        Ordering::Relaxed,
                    );
                    if let Some(share) = delivery_share(
                        std::time::Instant::now(),
                        fb.packets_received,
                        &counters,
                        &mut window,
                        &mut share_clocks,
                        bitrate_automatic,
                        wire_bytes,
                    ) {
                        // A share under the live rate is a retarget the encoder
                        // takes now; one above it is a ceiling the client still
                        // has to earn. A hand-back binds nothing: the path goes
                        // as a ceiling, and the release follows it.
                        let binds = counters.share.share_kbps() > 0;
                        let live = live_bitrate.load(Ordering::Relaxed);
                        if binds && live > share && bitrate_tx.send(share).is_err() {
                            break;
                        }
                        let ack = bitrate_ack(share, AckReason::Governor, ack_reason);
                        if v2io::send(&mut ctrl_send, &ack).await.is_err() {
                            break;
                        }
                        if !binds && ack_reason {
                            let release = bitrate_ack(NO_SHARE_KBPS, AckReason::Governor, true);
                            if v2io::send(&mut ctrl_send, &release).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
            // `select!` drops this future whenever a sibling fires; the reader keeps a partial
            // frame for the next call.
            frame = ctrl_recv.read_frame() => {
                let Ok((ty, body)) = frame else { break };
                if let Some(ev) = v2msg::decode_input_event(ty, &body) {
                    offer_edge(ev, &session_grants, &input_tx, &counters, &mut denied).await;
                } else if let Ok(req) = v2msg::decode::<Reconfigure>(ty, &body) {
                    let now = std::time::Instant::now();
                    // Same bound as the handshake: `> 0` alone acked a mode that cannot land.
                    let valid = crate::encode::validate_refresh(req.mode.refresh_hz).is_ok()
                        && crate::encode::validate_dimensions(
                            codec,
                            req.mode.width,
                            req.mode.height,
                        )
                        .is_ok();
                    let too_soon = last_accepted_switch
                        .is_some_and(|t| now.duration_since(t) < MIN_SWITCH_INTERVAL);
                    let ok = if !live_reconfig_ok {
                        // Backend cannot live-reconfigure (gamescope / synthetic
                        // / per-client-mode identity). Client keeps scaling.
                        tracing::info!(mode = ?req.mode,
                            "mode switch rejected (backend cannot live-reconfigure)");
                        false
                    } else if !valid {
                        tracing::warn!(mode = ?req.mode, "mode switch rejected (invalid dimensions)");
                        false
                    } else if too_soon {
                        tracing::warn!(mode = ?req.mode, "mode switch rejected (rate-limited)");
                        false
                    } else {
                        true
                    };
                    if ok {
                        active = req.mode;
                        last_accepted_switch = Some(now);
                        tracing::info!(mode = ?req.mode, "mode switch accepted");
                    }
                    let ack = Reconfigured { accepted: ok, mode: active };
                    if v2io::send(&mut ctrl_send, &ack).await.is_err() {
                        break;
                    }
                    if ok && reconfig_tx.send(req.mode).is_err() {
                        break;
                    }
                } else if let Ok(id) = v2msg::decode::<punktfunk_core::quic::PadIdentity>(ty, &body) {
                    if session_grants.load(Ordering::Relaxed) & punktfunk_core::quic::GRANT_GAMEPAD != 0
                        && input_tx.try_send(super::input::ClientInput::PadIdentity(id)).is_err()
                    {
                        tracing::warn!("pad identity dropped: input queue full");
                    }
                } else if let Ok(req) = v2msg::decode::<SetBitrate>(ty, &body) {
                    link.note_bitrate_ask(
                        req.bitrate_kbps,
                        live_bitrate.load(Ordering::Relaxed),
                    );
                    // Data plane rebuilds the encoder in place (first frame is
                    // an IDR with in-band SPS). PyroWave is pinned: ack the
                    // pin so a foreign client cannot AIMD it down. The one
                    // exception is the Automatic ramp's verdict, inside the
                    // window and lower, which becomes the pin.
                    let (resolved, why) = if codec == crate::encode::Codec::PyroWave {
                        let was_kbps = pyrowave_pin.kbps;
                        let resolved =
                            pyrowave_pin.resolve(req.bitrate_kbps, ramp_open.load(Ordering::SeqCst));
                        if resolved < was_kbps {
                            tracing::info!(
                                pin_kbps = was_kbps,
                                fit_kbps = resolved,
                                "PyroWave pin lowered to what the bring-up ramp measured"
                            );
                        } else {
                            tracing::info!(
                                requested_kbps = req.bitrate_kbps,
                                pinned_kbps = resolved,
                                "PyroWave session: mid-stream bitrate retarget refused (pinned)"
                            );
                        }
                        (resolved, AckReason::Pinned)
                    } else {
                        let mut want = resolve_bitrate_kbps(req.bitrate_kbps);
                        let mut why = AckReason::Granted;
                        // On a fat LAN nothing else stops the climb, and past
                        // the compute knee more bits deepen the miss. Hold a
                        // climb at the applied rate; descents pass. Held first
                        // because a rate the encoder never sees must not spend
                        // the ceiling's re-test.
                        let live = live_bitrate.load(Ordering::Relaxed);
                        if cadence_degraded.load(Ordering::Relaxed) && live != 0 && want > live {
                            tracing::info!(
                                requested_kbps = req.bitrate_kbps,
                                held_kbps = live,
                                // Why the hold: ABR-floor vs network look the
                                // same without this score.
                                behind_score = cadence_behind_score.load(Ordering::Relaxed),
                                "bitrate climb refused — encode is behind cadence"
                            );
                            want = live;
                            why = AckReason::Cadence;
                        }
                        // Ack is the client's climb base: never promise past a
                        // ceiling the encoder taught, unless its wait has run
                        // out and this ask is the re-test. A ceiling under the
                        // held rate binds tighter, and names the ack instead.
                        let (mut r, ceiling_why) = encoder_ceiling
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .resolve(want);
                        if r < want {
                            why = ceiling_why;
                        }
                        // The share this session holds on a path it is not
                        // alone on binds last: whatever else allows, it may not
                        // take a sibling's half.
                        let share = counters.share.share_kbps();
                        if share > 0 && r > share {
                            r = share;
                            why = AckReason::Governor;
                        }
                        (r, why)
                    };
                    tracing::debug!(
                        requested_kbps = req.bitrate_kbps,
                        resolved_kbps = resolved,
                        "mid-stream bitrate change requested"
                    );
                    let ack = bitrate_ack(resolved, why, ack_reason);
                    if v2io::send(&mut ctrl_send, &ack).await.is_err() {
                        break;
                    }
                    if bitrate_tx.send(resolved).is_err() {
                        break;
                    }
                } else if let Ok(ack) = v2msg::decode::<punktfunk_core::quic::ShardPayloadAck>(ty, &body) {
                    // Grow gate: packetizer may exceed the old size only after
                    // this ack. A dropped send means the watcher already ended.
                    tracing::info!(
                        shard_payload = ack.shard_payload,
                        "client acked shard-payload change"
                    );
                    let _ = shard_ack_tx.send(ack.shard_payload);
                } else if let Ok(req) = v2msg::decode::<ProbeShaped>(ty, &body) {
                    let open = ramp_open.load(Ordering::SeqCst);
                    if !probe_only
                        && !probe_spacing.admit(std::time::Instant::now(), is_ramp_length(&req), open)
                    {
                        tracing::warn!(
                            target_kbps = req.target_kbps,
                            "speed-test probe rejected (rate-limited)"
                        );
                        // The client holds its reports until a probe is answered.
                        let declined = super::stream::declined();
                        if v2io::send(&mut ctrl_send, &declined).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    tracing::info!(
                        target_kbps = req.target_kbps,
                        duration_ms = req.duration_ms,
                        "speed-test probe requested"
                    );
                    if probe_tx.send(req).is_err() {
                        break;
                    }
                } else if let Ok(probe) = v2msg::decode::<ClockProbe>(ty, &body) {
                    // t2/t3 in the session clock video pts leave in. Inline; no data-plane hop.
                    let t2_ns = clock.to_wire(now_ns());
                    let echo = ClockEcho {
                        t1_ns: probe.t1_ns,
                        t2_ns,
                        t3_ns: clock.to_wire(now_ns()),
                    };
                    if v2io::send(&mut ctrl_send, &echo).await.is_err() {
                        break;
                    }
                } else if let Ok(m) = v2msg::decode::<punktfunk_core::quic::CursorRenderMode>(ty, &body) {
                    // Data-plane edge-detects per tick (forward+exclude vs
                    // composite). Inert without the cursor cap.
                    cursor_client_draws.store(m.client_draws, Ordering::Relaxed);
                    tracing::info!(
                        client_draws = m.client_draws,
                        "cursor render mode set by client"
                    );
                } else if let Ok(ctl) = v2msg::decode::<ClipControl>(ty, &body) {
                    let granted = session_grants.load(Ordering::Relaxed)
                        & punktfunk_core::quic::GRANT_CLIPBOARD
                        != 0;
                    let (enabled, resolved_policy, reason) =
                        resolve_clip_control(pf_clipboard::policy(), granted, clip_available, ctl);
                    clip_enabled.store(enabled, Ordering::SeqCst);
                    // Enable re-announces the host clipboard; disable drops
                    // our selection. Inert handle: dropped send is fine.
                    let _ = clip_cmd_tx.send(ClipCoordCmd::SetEnabled(enabled));
                    tracing::info!(
                        enabled,
                        files = enabled
                            && resolved_policy & punktfunk_core::quic::CLIP_POLICY_FILES != 0,
                        "clipboard control"
                    );
                    let state = ClipState {
                        enabled,
                        policy: resolved_policy,
                        reason,
                    };
                    if v2io::send(&mut ctrl_send, &state).await.is_err() {
                        break;
                    }
                } else if let Ok(offer) = v2msg::decode::<ClipOffer>(ty, &body) {
                    // WRITE half of CLIPBOARD: installs a client selection on
                    // the host clipboard.
                    if clip_offer_permitted(
                        session_grants.load(Ordering::Relaxed),
                        clip_enabled.load(Ordering::SeqCst),
                    ) {
                        tracing::debug!(
                            seq = offer.seq,
                            kinds = offer.kinds.len(),
                            "clipboard offer from client"
                        );
                        let mimes = offer.kinds.iter().map(|k| k.mime.clone()).collect();
                        let _ = clip_cmd_tx.send(ClipCoordCmd::RemoteOffer {
                            seq: offer.seq,
                            mimes,
                        });
                    } else {
                        denied.note(GrantClass::Clipboard);
                    }
                } else {
                    tracing::debug!(ty, "control frame of a type this host does not read");
                }
            }
            result = probe_result_rx.recv() => {
                let Some(result) = result else { break };
                if v2io::send(&mut ctrl_send, &result).await.is_err() {
                    break;
                }
            }
            n = shard_change_rx.recv(), if !shard_change_closed => {
                // `None` is the watcher's bounded lifetime, not session end:
                // disable this branch or a closed mpsc busy-spins `select!`.
                let Some(n) = n else { shard_change_closed = true; continue };
                let msg = punktfunk_core::quic::ShardPayloadChanged { shard_payload: n };
                if v2io::send(&mut ctrl_send, &msg).await.is_err() {
                    break;
                }
            }
            changed = cursor_shape_rx.changed() => {
                // Err = encode loop gone, i.e. session end.
                if changed.is_err() {
                    break;
                }
                // ≤ ~58 KiB fits the u16 frame (`cursor_fwd` downscales).
                let shape = cursor_shape_rx.borrow_and_update().clone();
                let Some(shape) = shape else { continue };
                if v2io::send(&mut ctrl_send, &shape).await.is_err() {
                    break;
                }
            }
            outcome = launch_outcome_rx.recv(), if !launch_outcome_closed => {
                // `None` = every sender gone; disable the arm rather than spin.
                let Some(outcome) = outcome else { launch_outcome_closed = true; continue };
                tracing::info!(
                    outcome = outcome.kind.as_str(),
                    said = %outcome.message,
                    "told the client how its launch turned out"
                );
                if v2io::send(&mut ctrl_send, &outcome).await.is_err() {
                    break;
                }
            }
            state = audio_rx.recv(), if !audio_closed => {
                // `None` = every sender gone. Disable the arm; a closed mpsc is
                // perpetually ready and would spin `select!`.
                let Some(state) = state else { audio_closed = true; continue };
                if v2io::send(&mut ctrl_send, &state).await.is_err() {
                    break;
                }
            }
            msg = pad_rx.recv(), if !pad_slots_closed => {
                // Same closed-mpsc rule as the audio arm above. The input thread is
                // the only sender, and it ends with the session.
                let sent = match msg {
                    None => { pad_slots_closed = true; continue }
                    Some(super::input::PadToClient::Slots(s)) => v2io::send(&mut ctrl_send, &s).await,
                    Some(super::input::PadToClient::Feature(f)) => v2io::send(&mut ctrl_send, &f).await,
                };
                if sent.is_err() {
                    break;
                }
            }
            update = access_rx.recv(), if !access_closed => {
                // `None` = deadline/watch ended or never existed (`--open`):
                // disable the branch.
                match update {
                    Some(u) => {
                        // Clearing `clip_enabled` only stops host→client. The
                        // selection this device installed stays on the host
                        // clipboard until `SetEnabled(false)` drops it.
                        if u.grants & punktfunk_core::quic::GRANT_CLIPBOARD == 0 {
                            let _ = clip_cmd_tx.send(ClipCoordCmd::SetEnabled(false));
                        }
                        if v2io::send(&mut ctrl_send, &u).await.is_err() {
                            break;
                        }
                    }
                    None => access_closed = true,
                }
            }
            offer = clip_offer_rx.recv(), if !clip_offer_closed => {
                // Forward while sync is on — a race with a just-received
                // disable would leak a stale offer. `None` = coordinator gone.
                match offer {
                    Some(offer) => {
                        if clip_enabled.load(Ordering::SeqCst)
                            && v2io::send(&mut ctrl_send, &offer).await.is_err()
                        {
                            break;
                        }
                    }
                    None => clip_offer_closed = true,
                }
            }
            retarget = retarget_rx.recv() => {
                // Same `BitrateChanged` as `SetBitrate`. PyroWave is pinned
                // against client retargets, but a mode switch re-resolves the
                // pin (~1.6 bpp for the new pixel rate) and the live-rate
                // display otherwise stays on the old number.
                let Some((kbps, why)) = retarget else { break };
                // A re-resolve is the pin itself moving: the verdict bound
                // follows it, or a queued mode switch would let a stale pin
                // admit a raise.
                if codec == crate::encode::Codec::PyroWave {
                    pyrowave_pin.kbps = kbps;
                }
                tracing::info!(
                    kbps,
                    "encoder re-targeted by a pipeline rebuild — telling the client"
                );
                let ack = bitrate_ack(kbps, why, ack_reason);
                if v2io::send(&mut ctrl_send, &ack).await.is_err() {
                    break;
                }
            }
            gap = gap_rx.recv() => {
                // Client bitrate controller must drop the straddling report
                // window, not read our stall as congestion. After the fact:
                // `gap_ms` is measured.
                let Some(gap_ms) = gap else { break };
                link.note_gap();
                tracing::info!(
                    gap_ms,
                    "pipeline rebuilt in place — telling the client the stream had a gap"
                );
                if v2io::send(&mut ctrl_send, &PipelineGap { gap_ms })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            _ = link_tick.tick() => {
                emit_link(&mut link, &counters, &fec_target, &live_bitrate, &stats, peer);
            }
            delivered = reconfig_result_rx.recv() => {
                // Keep `active` truthful for later rejection echoes.
                let Some(d) = delivered else { break };
                active = d.mode;
                let sent = match tell(&mut stream_config, &d) {
                    Some(Tell::Config(cfg)) => v2io::send(&mut ctrl_send, &cfg).await,
                    Some(Tell::Correct(ack)) => v2io::send(&mut ctrl_send, &ack).await,
                    None => Ok(()),
                };
                if sent.is_err() {
                    break;
                }
            }
        }
    }
    // A session that ends mid-minute still reports what it had.
    emit_link(
        &mut link,
        &counters,
        &fec_target,
        &live_bitrate,
        &stats,
        peer,
    );
}

/// Close the window, log it, and append it to an armed capture.
///
/// The FEC and ABR bands are sampled here as well as per report window, so the line names the
/// rates a silent minute ran at rather than a zero it never measured.
fn emit_link(
    link: &mut crate::link_health::LinkWindow,
    counters: &crate::session_status::SessionCounters,
    fec_target: &AtomicU8,
    live_bitrate: &AtomicU32,
    stats: &crate::stats_recorder::StatsRecorder,
    peer: std::net::IpAddr,
) {
    let m = link.close(
        &counters.link,
        fec_target.load(Ordering::Relaxed),
        live_bitrate.load(Ordering::Relaxed),
    );
    crate::link_health::emit(&m, peer);
    if stats.is_armed() {
        stats.push_link(m);
    }
}

/// Operator policy (`None` = off), then CLIPBOARD grant (AND, never override),
/// then backend availability. Returns `(enabled, resolved_policy, reason)`.
///
/// A grant refusal still reports the operator policy bits so the client can
/// say "not permitted for this device" without greying the file toggle.
fn resolve_clip_control(
    policy: Option<u8>,
    granted: bool,
    clip_available: bool,
    ctl: ClipControl,
) -> (bool, u8, u8) {
    match policy {
        None => (false, 0, punktfunk_core::quic::CLIP_REASON_POLICY_DISABLED),
        Some(p) if !granted => (false, p, punktfunk_core::quic::CLIP_REASON_NOT_PERMITTED),
        Some(p) if ctl.enabled && !clip_available => (
            false,
            p,
            punktfunk_core::quic::CLIP_REASON_BACKEND_UNAVAILABLE,
        ),
        Some(p) => {
            let files_ok = p & punktfunk_core::quic::CLIP_POLICY_FILES != 0;
            let wants_files = ctl.flags & punktfunk_core::quic::CLIP_FLAG_FILES != 0;
            let reason = if wants_files && !files_ok {
                punktfunk_core::quic::CLIP_REASON_NO_FILES
            } else {
                punktfunk_core::quic::CLIP_REASON_OK
            };
            (ctl.enabled, p, reason)
        }
    }
}

/// LIVE CLIPBOARD grant ANDed with the last resolved sync state. Both are
/// read at offer time so a mid-session revoke closes this direction too.
fn clip_offer_permitted(grants: u32, clip_enabled: bool) -> bool {
    grants & punktfunk_core::quic::GRANT_CLIPBOARD != 0 && clip_enabled
}

/// A key edge off the control stream joins the datagram plane's queue: the same grant
/// gate and count, and a press drops when the input thread is behind. A release waits up
/// to [`RELEASE_WAIT`] for room: a lost one holds the key down on the host, and this lane
/// exists so none is lost. The lane keeps its order; the queue keeps it too.
async fn offer_edge(
    ev: InputEvent,
    grants: &AtomicU32,
    input_tx: &std::sync::mpsc::SyncSender<super::input::ClientInput>,
    counters: &crate::session_status::SessionCounters,
    denied: &mut crate::session_status::GrantDrops,
) {
    let class = punktfunk_core::quic::classify(ev.kind);
    if grants.load(Ordering::Relaxed) & class.bit() == 0 {
        denied.note(class);
        return;
    }
    counters.input_events.fetch_add(1, Ordering::Relaxed);
    let mut ev = ev;
    // KEY_FLAG_SEMANTIC_VK is in-process (GameStream ingest). Strip it from the wire.
    if matches!(ev.kind, InputKind::KeyDown | InputKind::KeyUp) {
        ev.flags &= !crate::inject::KEY_FLAG_SEMANTIC_VK;
    }
    let release = matches!(
        ev.kind,
        InputKind::KeyUp | InputKind::MouseButtonUp | InputKind::TouchUp
    );
    let deadline = tokio::time::Instant::now() + RELEASE_WAIT;
    let mut msg = super::input::ClientInput::Event(ev);
    loop {
        match input_tx.try_send(msg) {
            Err(std::sync::mpsc::TrySendError::Full(back))
                if release && tokio::time::Instant::now() < deadline =>
            {
                msg = back;
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                counters.input_dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Ok(()) | Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return,
        }
    }
}

/// A blocking pad create or the 200 ms "owned elsewhere" wait stalls the input thread; a
/// release outlasts both. Longer, and the session is gone anyway.
const RELEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::ProbeRequest;
    use punktfunk_core::quic::{
        CLIP_FLAG_FILES, CLIP_POLICY_FILES, CLIP_POLICY_TEXT, CLIP_REASON_BACKEND_UNAVAILABLE,
        CLIP_REASON_NOT_PERMITTED, CLIP_REASON_NO_FILES, CLIP_REASON_OK,
        CLIP_REASON_POLICY_DISABLED, GRANT_ALL, GRANT_CLIPBOARD,
    };

    /// A window or ask number is acted on once: a repeat, an older reordered copy and `0` are
    /// skipped, and the client's wrap past the top (which skips `0`) still reads as newer.
    #[test]
    fn each_feedback_number_is_taken_once() {
        let mut last = 0;
        assert!(!take_newer(0, &mut last, u16::MAX.into()), "0 asks nothing");
        assert!(take_newer(1, &mut last, u16::MAX.into()));
        assert!(!take_newer(1, &mut last, u16::MAX.into()), "a copy");
        assert!(take_newer(3, &mut last, u16::MAX.into()), "one copy lost");
        assert!(
            !take_newer(2, &mut last, u16::MAX.into()),
            "an older copy, reordered"
        );
        last = 65_535;
        assert!(take_newer(1, &mut last, u16::MAX.into()), "past the wrap");
        let mut w = u32::MAX;
        assert!(take_newer(1, &mut w, u32::MAX));
        assert!(!take_newer(u32::MAX, &mut w, u32::MAX));
    }

    /// A client that takes configs hears each new epoch once; one that does not hears only
    /// the corrections.
    #[test]
    fn a_delivered_mode_is_a_config_or_a_correction() {
        use super::super::wiring::Delivered;
        let mode = |width| punktfunk_core::Mode {
            width,
            height: 720,
            refresh_hz: 60,
        };
        let d = |epoch, width, corrects| Delivered {
            mode: mode(width),
            epoch,
            corrects,
        };
        let mut held = Some(v2msg::StreamConfig {
            epoch: 0,
            mode: mode(1280),
            ..Default::default()
        });
        assert_eq!(tell(&mut held, &d(0, 1280, true)), None, "a failed rebuild");
        let Some(Tell::Config(cfg)) = tell(&mut held, &d(1, 1920, false)) else {
            panic!("a new epoch is configured")
        };
        assert_eq!((cfg.epoch, cfg.mode.width), (1, 1920));
        assert_eq!(tell(&mut held, &d(1, 1920, true)), None, "already held");

        let mut none = None;
        assert_eq!(tell(&mut none, &d(1, 1920, false)), None);
        assert_eq!(
            tell(&mut none, &d(2, 800, true)),
            Some(Tell::Correct(Reconfigured {
                accepted: true,
                mode: mode(800)
            }))
        );
    }

    const ON: ClipControl = ClipControl {
        enabled: true,
        flags: 0,
    };

    /// One wire packet, and the rate both modelled sessions open at.
    const WIRE: u64 = 1_448;
    const START_KBPS: u32 = 12_000;

    /// One modelled session: the [`punktfunk_core::abr::Driver`] a real client
    /// runs, the registry entry and counters the host keeps for it, and the
    /// share window its reports close.
    struct Peer {
        abr: punktfunk_core::abr::Driver,
        counters: Arc<crate::session_status::SessionCounters>,
        /// Encoder target, the same Arc the governor reads as `current_kbps`.
        rate: Arc<AtomicU32>,
        window: ShareWindow,
        clocks: ShareClocks,
        stats: punktfunk_core::stats::Stats,
        egress: u64,
        arrived: u64,
        /// The share standing over this session, and every one it was told.
        share: u32,
        told: Vec<(u64, u32)>,
        _live: crate::session_status::LiveSessionGuard,
    }

    fn peer(name: &str, at: std::net::IpAddr, base: std::time::Instant) -> Peer {
        let (live, counters, rate) =
            crate::session_status::tests::fake_member(name, at, START_KBPS);
        Peer {
            abr: punktfunk_core::abr::Driver::new(
                punktfunk_core::abr::DriverConfig {
                    start_kbps: START_KBPS,
                    ceiling_cap_kbps: None,
                    stream_cap_kbps: 200_000,
                    refresh_hz: 60,
                    codec: punktfunk_core::quic::CODEC_HEVC,
                    bit_depth: 8,
                    chroma_format: punktfunk_core::quic::CHROMA_IDC_420,
                    audio_reserved_kbps: 256,
                    marks_repeats: true,
                    probe: false,
                    probe_target_kbps: None,
                    ramp: false,
                    probe_only: false,
                    pin_kbps: None,
                },
                base,
            ),
            counters,
            rate,
            window: ShareWindow::new(base, 0),
            clocks: ShareClocks::new(base),
            stats: punktfunk_core::stats::Stats::default(),
            egress: 0,
            arrived: 0,
            share: 0,
            told: Vec::new(),
            _live: live,
        }
    }

    impl Peer {
        /// One millisecond of this session: the host puts its encoder rate on
        /// the wire, the link carries what it has room for, and whatever the
        /// driver asks to send reaches the host's own handling of it.
        fn step(&mut self, ms: u64, at: std::time::Instant, link_kbps: u32) {
            let offer = u64::from(self.rate.load(Ordering::Relaxed));
            self.egress += offer * 125 / 1_000;
            self.counters.link.publish_egress_bytes(self.egress);
            self.arrived += offer.min(u64::from(link_kbps)) * 125 / 1_000;
            self.stats.packets_received = self.arrived / WIRE;
            self.stats.bytes_received = self.arrived;
            if ms % 16 == 0 {
                self.stats.frames_completed += 1;
                self.abr.on_au(false);
            }
            self.abr.on_stats(&self.stats);
            for action in self.abr.tick(at).actions {
                match action {
                    punktfunk_core::abr::Action::Report {
                        packets_received, ..
                    } => {
                        let Some(share) = delivery_share(
                            at,
                            packets_received,
                            &self.counters,
                            &mut self.window,
                            &mut self.clocks,
                            true,
                            WIRE,
                        ) else {
                            continue;
                        };
                        self.share = share;
                        self.told.push((ms, share));
                        // As the branch does: a share under the live rate is a
                        // retarget the encoder takes now.
                        if share > 0 && self.rate.load(Ordering::Relaxed) > share {
                            self.rate.store(share, Ordering::Relaxed);
                        }
                    }
                    // As the `SetBitrate` branch does: the share binds last.
                    punktfunk_core::abr::Action::SetBitrate(kbps) => {
                        let want = if self.share > 0 {
                            kbps.min(self.share)
                        } else {
                            kbps
                        };
                        self.rate.store(want, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Eighteen seconds of a path that falls twice under these sessions: nine
    /// megabits, then five, then three, halved because two of them share it.
    fn falls_twice(peers: &mut [Peer], base: std::time::Instant) {
        for ms in 0..18_000 {
            let link = match ms {
                0..6_000 => 9_000,
                6_000..12_000 => 5_000,
                _ => 3_000,
            } / 2;
            for p in peers.iter_mut() {
                p.step(ms, base + std::time::Duration::from_millis(ms), link);
            }
        }
    }

    /// The sequence a real client sends, through the host's own handling of it:
    /// two sessions at one address over a path that falls twice under them.
    ///
    /// Each is told a share again at every fall, because the driver closes the
    /// host's share window every report window. The third session is alone at
    /// its own address and is never told anything.
    #[test]
    fn a_real_clients_reports_keep_dividing_a_shared_path() {
        let _registry = crate::session_status::tests::registry_lock();
        let base = std::time::Instant::now();
        let shared: std::net::IpAddr = "203.0.113.41".parse().unwrap();
        let mut peers = [
            peer("phone", shared, base),
            peer("pc", shared, base),
            peer("tv", "203.0.113.42".parse().unwrap(), base),
        ];
        falls_twice(&mut peers, base);
        for p in &peers[..2] {
            assert!(
                p.told.len() >= 3,
                "three falls, {} shares: {:?}",
                p.told.len(),
                p.told
            );
            assert!(
                p.told.last().is_some_and(|&(ms, _)| ms > 12_000),
                "the governor stopped early: {:?}",
                p.told
            );
        }
        assert!(
            peers[2].told.is_empty(),
            "a session alone at its address was told {:?}",
            peers[2].told
        );
    }

    /// A client whose `ClientHello` carried no `EXT_TAG_ABR` gets an ack without a reason,
    /// whatever the host knows about the refusal: a 0.43 client rejects any other ack.
    #[test]
    fn a_client_that_did_not_ask_gets_no_reason() {
        use punktfunk_core::quic::v2::msg::{decode, V2Message};
        let trip = |m: &BitrateChanged| {
            decode::<BitrateChanged>(BitrateChanged::TYPE, &m.fields().into_body()).unwrap()
        };
        for why in [
            AckReason::Granted,
            AckReason::EncoderLimit,
            AckReason::Cadence,
            AckReason::Pinned,
        ] {
            let old = bitrate_ack(41_852, why, false);
            assert_eq!(trip(&old).reason, None);
            let new = bitrate_ack(41_852, why, true);
            assert_eq!(trip(&new).reason, Some(why));
            assert_eq!(new.bitrate_kbps, old.bitrate_kbps);
        }
    }

    #[test]
    fn clip_resolution_three_way() {
        let both = CLIP_POLICY_TEXT | CLIP_POLICY_FILES;

        assert_eq!(
            resolve_clip_control(None, false, true, ON),
            (false, 0, CLIP_REASON_POLICY_DISABLED)
        );
        assert_eq!(
            resolve_clip_control(None, true, true, ON),
            (false, 0, CLIP_REASON_POLICY_DISABLED)
        );

        assert_eq!(
            resolve_clip_control(Some(both), false, true, ON),
            (false, both, CLIP_REASON_NOT_PERMITTED)
        );

        assert_eq!(
            resolve_clip_control(Some(both), true, false, ON),
            (false, both, CLIP_REASON_BACKEND_UNAVAILABLE)
        );
        assert_eq!(
            resolve_clip_control(
                Some(CLIP_POLICY_TEXT),
                true,
                true,
                ClipControl {
                    enabled: true,
                    flags: CLIP_FLAG_FILES,
                }
            ),
            (true, CLIP_POLICY_TEXT, CLIP_REASON_NO_FILES)
        );
        assert_eq!(
            resolve_clip_control(Some(both), true, true, ON),
            (true, both, CLIP_REASON_OK)
        );
    }

    #[test]
    fn clip_offer_needs_the_live_grant() {
        assert!(clip_offer_permitted(GRANT_ALL, true));

        assert!(!clip_offer_permitted(GRANT_ALL & !GRANT_CLIPBOARD, true));
        assert!(!clip_offer_permitted(0, true));

        assert!(!clip_offer_permitted(GRANT_ALL & !GRANT_CLIPBOARD, false));
        assert!(!clip_offer_permitted(GRANT_ALL, false));
    }

    /// The bring-up exemption is bounded twice: the step has to be short, and
    /// it has to be in the window or just after an in-window step. An 800 ms
    /// burst is spaced like any other, open window or not, and a short step
    /// with no ramp behind it is spaced too.
    #[test]
    fn only_a_short_step_of_the_ramp_skips_the_spacing() {
        let req = |duration_ms| -> ProbeShaped {
            ProbeRequest {
                target_kbps: 40_000,
                duration_ms,
            }
            .into()
        };
        assert!(is_ramp_length(&req(25)));
        assert!(is_ramp_length(&req(super::stream::RAMP_STEP_MAX_MS)));
        assert!(!is_ramp_length(&req(super::stream::RAMP_STEP_MAX_MS + 1)));

        let t0 = std::time::Instant::now();
        let at = |ms| t0 + std::time::Duration::from_millis(ms);
        let mut spacing = ProbeSpacing::default();
        assert!(spacing.admit(at(0), false, true), "a long burst: served");
        assert!(
            !spacing.admit(at(10), false, true),
            "and spaced, window or not"
        );
        let mut spacing = ProbeSpacing::default();
        assert!(
            spacing.admit(at(0), true, false),
            "a short step, no ramp: served"
        );
        assert!(
            !spacing.admit(at(10), true, false),
            "and it started the spacing"
        );
    }

    /// A ramp cut short by the first frame asks for its burst two seconds
    /// later. The steps before it — including a re-ask that reached the host
    /// just after the window closed — must not start the spacing, or that
    /// burst is refused and the session gets no measurement at all.
    #[test]
    fn ramp_steps_leave_the_spacing_to_the_burst_after_them() {
        let t0 = std::time::Instant::now();
        let at = |ms| t0 + std::time::Duration::from_millis(ms);
        let mut spacing = ProbeSpacing::default();
        for ms in [0, 72] {
            assert!(spacing.admit(at(ms), true, true), "an in-window step");
        }
        // The rig: the window closed at 103 ms, the re-ask arrived at 157 ms.
        assert!(
            spacing.admit(at(157), true, false),
            "a re-ask across the close"
        );
        assert!(
            spacing.admit(at(2_628), false, false),
            "the burst after the ramp"
        );
        assert!(
            !spacing.admit(at(5_000), false, false),
            "spaced from that burst"
        );
        assert!(
            !spacing.admit(at(5_010), true, false),
            "past the grace a short step is spaced"
        );
        assert!(spacing.admit(at(12_629), false, false), "ten seconds on");
    }

    /// The ramp's verdict ask — lower than the pin, inside the bring-up
    /// window — becomes the pin, and the ack carries it.
    #[test]
    fn a_ramp_verdict_under_the_pin_lowers_it() {
        let mut pin = PyroWavePin {
            kbps: 800_000,
            automatic: true,
            fit_taken: false,
        };
        assert_eq!(pin.resolve(340_000, true), 340_000);
        assert_eq!(pin.kbps, 340_000, "the pin moved to the measured rate");
    }

    /// Once the verdict landed the session is pinned again: a further ask —
    /// even a lower one — is refused with the pin as it now stands.
    #[test]
    fn the_verdict_ask_comes_once() {
        let mut pin = PyroWavePin {
            kbps: 800_000,
            automatic: true,
            fit_taken: false,
        };
        pin.resolve(340_000, true);
        assert_eq!(
            pin.resolve(200_000, true),
            340_000,
            "a second lower ask is refused at the lowered pin"
        );
        assert_eq!(
            pin.resolve(900_000, true),
            340_000,
            "and a raise over the old pin is refused the same"
        );
    }

    /// A raise inside the window is refused and does not spend the verdict:
    /// the ramp's own ask, landing behind it, still lowers the pin.
    #[test]
    fn the_pin_never_raises_and_a_refusal_spends_nothing() {
        let mut pin = PyroWavePin {
            kbps: 800_000,
            automatic: true,
            fit_taken: false,
        };
        assert_eq!(pin.resolve(900_000, true), 800_000, "a raise is refused");
        assert_eq!(pin.resolve(800_000, true), 800_000, "as is the pin itself");
        assert_eq!(
            pin.resolve(340_000, true),
            340_000,
            "the verdict still fits after them"
        );
    }

    /// The window closes with the bring-up: a verdict that lands after it is
    /// an ordinary ask, refused like every other.
    #[test]
    fn a_late_verdict_finds_the_window_closed() {
        let mut pin = PyroWavePin {
            kbps: 800_000,
            automatic: true,
            fit_taken: false,
        };
        assert_eq!(
            pin.resolve(340_000, false),
            800_000,
            "ramp_open closed: the pin stands"
        );
        // And it did not spend the verdict — an ask inside the window can
        // still follow a refused one, though the window itself is gone.
        assert!(!pin.fit_taken);
    }

    /// An explicit-rate PyroWave session — resolved to the same pin — gets no
    /// window at all: its asks are refused inside the window too, and a zero
    /// ask on the wire is no verdict.
    #[test]
    fn only_an_automatic_sessions_verdict_fits() {
        let mut explicit = PyroWavePin {
            kbps: 800_000,
            automatic: false,
            fit_taken: false,
        };
        assert_eq!(explicit.resolve(340_000, true), 800_000);
        let mut automatic = PyroWavePin {
            kbps: 800_000,
            automatic: true,
            fit_taken: false,
        };
        assert_eq!(
            automatic.resolve(0, true),
            800_000,
            "a zero ask is not a measurement"
        );
    }

    /// A key edge off the control stream takes the datagram path's grant gate, count and
    /// queue; the in-process flag never survives the wire; a full queue drops a press and
    /// counts it, and holds a release until there is room.
    #[tokio::test]
    async fn an_edge_off_the_control_stream_joins_the_input_queue() {
        use punktfunk_core::quic::{GRANT_ALL, GRANT_PRESET_CONTROLLER_ONLY};
        let (tx, rx) = std::sync::mpsc::sync_channel::<super::super::input::ClientInput>(1);
        let counters = crate::session_status::SessionCounters::default();
        let mut denied = crate::session_status::GrantDrops::new(crate::events::Plane::Native);
        let key = InputEvent {
            kind: InputKind::KeyDown,
            _pad: [0; 3],
            code: 0x41,
            x: 0,
            y: 0,
            flags: crate::inject::KEY_FLAG_SEMANTIC_VK | 4,
        };
        offer_edge(
            key,
            &AtomicU32::new(GRANT_PRESET_CONTROLLER_ONLY),
            &tx,
            &counters,
            &mut denied,
        )
        .await;
        assert!(rx.try_recv().is_err(), "no keyboard grant, no event");
        assert_eq!(counters.input_events.load(Ordering::Relaxed), 0);

        let grants = AtomicU32::new(GRANT_ALL);
        offer_edge(key, &grants, &tx, &counters, &mut denied).await;
        match rx.try_recv() {
            Ok(super::super::input::ClientInput::Event(ev)) => {
                assert_eq!((ev.kind, ev.code, ev.flags), (InputKind::KeyDown, 0x41, 4))
            }
            other => panic!("the edge, unflagged: {}", other.is_ok()),
        }
        offer_edge(key, &grants, &tx, &counters, &mut denied).await;
        offer_edge(key, &grants, &tx, &counters, &mut denied).await;
        assert_eq!(counters.input_events.load(Ordering::Relaxed), 3);
        assert_eq!(counters.input_dropped.load(Ordering::Relaxed), 1);

        // The queue is full; a release waits for the input thread instead of dropping.
        let drain = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let _ = rx.recv();
            rx.recv().map(|m| match m {
                super::super::input::ClientInput::Event(ev) => ev.kind,
                _ => InputKind::KeyDown,
            })
        });
        let up = InputEvent {
            kind: InputKind::KeyUp,
            ..key
        };
        offer_edge(up, &grants, &tx, &counters, &mut denied).await;
        assert_eq!(drain.join().unwrap().ok(), Some(InputKind::KeyUp));
        assert_eq!(counters.input_dropped.load(Ordering::Relaxed), 1);
    }
}
