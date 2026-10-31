//! Native mid-stream control task.
//!
//! After handshake the control stream stays open. This task is its sole writer:
//! inbound client requests share a `select!` with outbound probe results, mode
//! corrections, bitrate retargets, pipeline gaps, shard-payload changes, cursor
//! shapes, clipboard offers, and access updates. Validated changes go to the
//! data-plane thread over the session's mpsc bridges.
//!
//! `run` is the `select!` over a `ControlTask`: one method per arm here, one per inbound
//! message in `inbound.rs`, the `SetBitrate` answer in `rate.rs`. Every loss report, RFI and
//! keyframe ask lands here, so the per-minute `link health` line ([`crate::link_health`]) is
//! counted and emitted here too.
//!
//! `select!` drops the inbound read future whenever a sibling fires, so framing uses a
//! [`FrameReader`](punktfunk_core::quic::v2::io::FrameReader). An arm whose sender can drop
//! mid-session is disabled on `None` (`Closed`), or a closed mpsc busy-spins.
//!
//! Evidence: `design/shard-payload-reneg.md`, `design/per-client-access.md`,
//! `design/clipboard-and-file-transfer.md`.

mod inbound;
mod rate;

use super::*;
use crate::native::bitrate::{pyrowave_bounds_kbps, UnrecoveredRun};
use pf_clipboard::ClipCoordCmd;
use punktfunk_core::abr::governor::{ShareWindow, NO_SHARE_KBPS};
use punktfunk_core::quic::v2::{dgram::Feedback, io as v2io, msg as v2msg};
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

/// The client's shape, for the send loop; logged when it moves. A client asks for the
/// wake shape or nothing: line rate and the smooth spread are the operator's. Returns
/// whether the wake shape is on.
fn apply_shape(slot: &std::sync::atomic::AtomicU8, asked: u8) -> bool {
    use crate::send_pacing::Shape;
    let shape = match Shape::from_u8(asked) {
        Shape::Wake => Shape::Wake,
        _ => Shape::Auto,
    };
    if slot.swap(shape as u8, Ordering::Relaxed) != shape as u8 {
        tracing::info!(?shape, "client changed the delivery shape");
    }
    shape == Shape::Wake
}

/// A probe's target, the bring-up ramp's steps included, under 0.9 `L_hard` when a port is
/// known. The result reports what was offered, so a bounded round says so itself.
fn probe_bound(target_kbps: u32, ports: crate::send_pacing::Ports) -> u32 {
    match ports.hard_kbps() {
        0 => target_kbps,
        p => target_kbps.min(p / 10 * 9),
    }
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
    /// The client's decodable codecs, its preference and what this source encodes: what a
    /// codec ask picks among.
    pub(super) client_codecs: u8,
    pub(super) preferred_codec: u8,
    pub(super) host_codecs: u8,
    /// The client rebuilds its decoder on a format epoch, and this session owns its capture.
    pub(super) format_change_ok: bool,
    pub(super) adaptive_fec: bool,
    pub(super) session_bitrate_kbps: u32,
    /// Automatic bitrate, so the shared-path governor may move this session. A
    /// client-set rate is never touched.
    pub(super) bitrate_automatic: bool,
    /// PyroWave session whose client asked Automatic (`Hello.bitrate_kbps ==
    /// 0`): it adapts between its floor and the pin. An explicit one runs at
    /// the pin, which its bring-up ramp may lower once while `ramp_open` holds.
    pub(super) pyrowave_automatic: bool,
    /// PyroWave's bits per pixel for this session; a delivered mode re-pins at it.
    pub(super) pyrowave_bpp: f64,
    /// The session's `PUNKTFUNK_PYROWAVE_MAX_MBPS` cap, kbps, over every re-pin.
    pub(super) pyrowave_max_kbps: Option<u32>,
    /// One wire packet, bytes. Turns the client's delivery count into the rate
    /// the governor divides, and the shard the parity rule sizes against.
    pub(super) wire_bytes: u64,
    /// Audio reservation out of the wire budget. With [`Self::wire_bytes`] and
    /// the live mode it is the frame adaptive FEC has to protect.
    pub(super) audio_kbps: u32,
    /// Client set `EXT_ABR_ACK_REASON` in its `ClientHello`: its `BitrateChanged`
    /// may carry the reason byte. Clear for every shipped client, which rejects
    /// a longer ack, and for every client behind a host without `HOST_CAP2_EXT`.
    pub(super) ack_reason: bool,
    /// The client's feedback datagrams, from the session's datagram reader.
    pub(super) feedback_rx: tokio::sync::mpsc::Receiver<punktfunk_core::quic::v2::dgram::Feedback>,
    /// Weak, so the channel still closes with the datagram reader. An older client's
    /// control-stream reports join the datagram's feedback through it.
    pub(super) feedback_tx:
        tokio::sync::mpsc::WeakSender<punktfunk_core::quic::v2::dgram::Feedback>,
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
    /// The audio thread's format changes, each before its first datagram.
    pub(super) audio_config_rx: tokio::sync::mpsc::UnboundedReceiver<v2msg::AudioConfig>,
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

/// A config for an epoch, mode or format the client does not hold yet, else a correction when
/// the client was told another mode. `stream_config` is what the client holds.
fn tell(
    stream_config: &mut Option<v2msg::StreamConfig>,
    d: &super::wiring::Delivered,
) -> Option<Tell> {
    match stream_config {
        Some(cfg) if (cfg.epoch, cfg.mode, cfg.format()) != (d.epoch, d.mode, d.format) => {
            (cfg.epoch, cfg.mode) = (d.epoch, d.mode);
            cfg.set_format(d.format);
            Some(Tell::Config(*cfg))
        }
        Some(_) => None,
        None => d.corrects.then_some(Tell::Correct(Reconfigured {
            accepted: true,
            mode: d.mode,
            exclude_codecs: 0,
        })),
    }
}

/// The codec a codec ask moves to: the client's preference among what it has left that this
/// host encodes, never PyroWave, whose wire framing is another shape. `None` keeps the stream.
fn next_codec(
    current: crate::encode::Codec,
    left: u8,
    host: u8,
    preferred: u8,
) -> Option<crate::encode::Codec> {
    if current == crate::encode::Codec::PyroWave {
        return None;
    }
    // `resolve_codec` reads an empty set as an older client's HEVC; here it is nothing left.
    let left = left & !punktfunk_core::quic::CODEC_PYROWAVE;
    if left == 0 {
        return None;
    }
    let bit = punktfunk_core::quic::resolve_codec(left, host, preferred)?;
    Some(crate::encode::codec_from_wire(bit)).filter(|to| *to != current)
}

/// Backstop against Reconfigure spam. Data-plane drain-to-newest already coalesces a resize
/// drag; 500 ms is half the client's 1 s self-limit.
const MIN_SWITCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// The control stream or a data-plane channel is gone: the task ends.
struct Ended;

/// What every arm and message handler returns: go on, or end the task.
type Step = Result<(), Ended>;

impl From<std::io::Error> for Ended {
    fn from(_: std::io::Error) -> Self {
        Ended
    }
}

impl<T> From<std::sync::mpsc::SendError<T>> for Ended {
    fn from(_: std::sync::mpsc::SendError<T>) -> Self {
        Ended
    }
}

/// The arms whose sender may drop mid-session. A closed mpsc is perpetually ready with `None`,
/// so its arm is disabled once it reads one.
#[derive(Default)]
struct Closed {
    /// The wire-MTU watcher's bounded lifetime, not session end.
    shard_change: bool,
    /// The launch site drops its sender when the session ends.
    launch_outcome: bool,
    audio: bool,
    /// The audio thread drops its sender when it ends; so does a client without epochs.
    audio_config: bool,
    /// The stream thread holds the sender and ends first.
    screen_tell: bool,
    /// The input thread is the only sender, and it ends with the session.
    pad_slots: bool,
    /// `--open` anonymous sessions never spawn deadline/watch; the sender drops at once.
    access: bool,
    /// The clipboard coordinator is gone.
    clip_offer: bool,
}

/// One session's control task: the [`Task`] it was handed and what the `select!` keeps
/// between arms. One method per arm here, one per inbound message in `inbound.rs`.
struct ControlTask {
    task: Task,
    closed: Closed,
    /// First-of-class `warn!` for grant drops: a revoked client spamming the gated messages
    /// must not flood the log.
    denied: crate::session_status::GrantDrops,
    /// The mode the client holds: every `Reconfigured` this task sends carries it, and
    /// adaptive FEC sizes a frame at its refresh.
    active: punktfunk_core::Mode,
    /// What the client holds; `None` for a client that takes no configs.
    held_config: Option<v2msg::StreamConfig>,
    pyrowave_pin: rate::PyroWavePin,
    last_accepted_switch: Option<std::time::Instant>,
    /// Codecs this session's client asked off, and the ask the stream thread has not answered.
    excluded: u8,
    asked: u8,
    probe_spacing: ProbeSpacing,
    /// An RFI ask is a frame parity could not repair; the report that closes the window
    /// carries only what parity did repair.
    unrecovered: UnrecoveredRun,
    /// `None` when `PUNKTFUNK_GSO` pins the offload on: an A/B run must not downshift itself.
    offload_guard: Option<OffloadGuard>,
    /// The link's loss over a horizon one report window cannot see, which is what sizes
    /// parity against the frame this session's budget buys.
    fec_horizon: punktfunk_core::abr::budget::LossHorizon,
    /// What this session offered and what reached it over the last window, read at one
    /// boundary so a shortfall describes one stretch of link, and the clocks an up-move rides.
    window: ShareWindow,
    share_clocks: ShareClocks,
    /// One `link health` line a minute, ticking whether or not anything arrived: a reader
    /// must be able to tell a clean minute from a host that stopped logging.
    link: crate::link_health::LinkWindow,
    link_tick: tokio::time::Interval,
    /// Feedback numbers acted on so far; a repeat or a reordered older copy is skipped.
    last_window: u32,
    last_ask: u32,
    retired: punktfunk_core::quic::v2::dgram::RetiredFeedback,
}

impl Task {
    /// PyroWave's floor and pin at `mode`, at the session's bits per pixel and cap. The
    /// Welcome resolved the pin for the first mode; the floor comes from the same rule.
    fn pyrowave_bounds(&self, mode: &punktfunk_core::Mode) -> (u32, u32) {
        let chroma = if self.stream_config.chroma_format == punktfunk_core::quic::CHROMA_IDC_444 {
            crate::encode::ChromaFormat::Yuv444
        } else {
            crate::encode::ChromaFormat::Yuv420
        };
        pyrowave_bounds_kbps(
            mode,
            chroma,
            self.stream_config.bit_depth,
            self.pyrowave_bpp,
            self.pyrowave_max_kbps,
        )
    }
}

/// Ends when the control stream closes or a data-plane channel drops. Each arm's future is
/// cancel-safe: `select!` drops the losers every turn and the next turn makes them again.
pub(super) async fn run(task: Task) {
    let mut c = ControlTask::new(task);
    // `interval` fires immediately; the first tick would close an empty window at 0 s.
    c.link_tick.tick().await;
    // The client reads this host's link facts off the first config, anchored or not.
    let first = c.task.stream_config;
    if c.send(&first).await.is_err() {
        return;
    }
    loop {
        let step = tokio::select! {
            fb = c.task.feedback_rx.recv() => c.on_feedback(fb).await,
            // The reader keeps a partial frame for the next call.
            frame = c.task.ctrl_recv.read_frame() => c.on_frame(frame).await,
            result = c.task.ends.probe_result_rx.recv() => c.on_probe_result(result).await,
            n = c.task.ends.shard_change_rx.recv(), if !c.closed.shard_change => {
                c.on_shard_change(n).await
            }
            changed = c.task.ends.cursor_shape_rx.changed() => c.on_cursor_shape(changed).await,
            outcome = c.task.launch_outcome_rx.recv(), if !c.closed.launch_outcome => {
                c.on_launch_outcome(outcome).await
            }
            state = c.task.audio_rx.recv(), if !c.closed.audio => c.on_audio_state(state).await,
            cfg = c.task.audio_config_rx.recv(), if !c.closed.audio_config => {
                c.on_audio_config(cfg).await
            }
            tell = c.task.ends.screen_tell_rx.recv(), if !c.closed.screen_tell => {
                c.on_screen_tell(tell).await
            }
            msg = c.task.pad_rx.recv(), if !c.closed.pad_slots => c.on_pad(msg).await,
            update = c.task.access_rx.recv(), if !c.closed.access => c.on_access(update).await,
            offer = c.task.clip.offer_rx.recv(), if !c.closed.clip_offer => {
                c.on_host_clip_offer(offer).await
            }
            retarget = c.task.ends.retarget_rx.recv() => c.on_retarget(retarget).await,
            gap = c.task.ends.gap_rx.recv() => c.on_gap(gap).await,
            _ = c.link_tick.tick() => {
                c.emit_link();
                Ok(())
            }
            outcome = c.task.ends.format_result_rx.recv() => c.on_format_result(outcome).await,
            delivered = c.task.ends.reconfig_result_rx.recv() => c.on_delivered(delivered).await,
        };
        if step.is_err() {
            break;
        }
    }
    // A session that ends mid-minute still reports what it had.
    c.emit_link();
}

impl ControlTask {
    fn new(task: Task) -> Self {
        let now = std::time::Instant::now();
        let pyrowave_pin = rate::PyroWavePin {
            kbps: task.session_bitrate_kbps,
            floor: if task.codec == crate::encode::Codec::PyroWave {
                task.pyrowave_bounds(&task.initial_mode)
                    .0
                    .min(task.session_bitrate_kbps)
            } else {
                0
            },
            automatic: task.pyrowave_automatic,
            fit_taken: false,
        };
        let offload_guard =
            (!punktfunk_core::transport::shared::offload_pinned()).then(|| OffloadGuard {
                switch: task.shared.offload_switch.clone(),
                over: 0,
            });
        let mut link_tick = tokio::time::interval(std::time::Duration::from_secs(60));
        link_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ControlTask {
            closed: Closed::default(),
            denied: crate::session_status::GrantDrops::new(task.plane),
            active: task.initial_mode,
            held_config: task.anchored.then_some(task.stream_config),
            pyrowave_pin,
            last_accepted_switch: None,
            excluded: 0,
            asked: 0,
            probe_spacing: ProbeSpacing::default(),
            unrecovered: UnrecoveredRun::default(),
            offload_guard,
            fec_horizon: Default::default(),
            window: ShareWindow::new(now, task.counters.link.egress_bytes()),
            share_clocks: ShareClocks::new(now),
            link: crate::link_health::LinkWindow::new(&task.counters.link),
            link_tick,
            last_window: 0,
            last_ask: 0,
            retired: Default::default(),
            task,
        }
    }

    async fn send<M: v2msg::V2Message>(&mut self, msg: &M) -> Step {
        v2io::send(&mut self.task.ctrl_send, msg)
            .await
            .map_err(Ended::from)
    }

    /// One feedback datagram: a further screen's keyframe ask, else the first screen's link
    /// facts, then its recovery ask and its report window, each number acted on once.
    async fn on_feedback(&mut self, fb: Option<Feedback>) -> Step {
        // The datagram reader ends with the connection.
        let fb = fb.ok_or(Ended)?;
        let shared = &self.task.shared;
        // A further screen's datagram carries its keyframe ask and nothing of the first
        // screen's window. Its loop coalesces repeats.
        if fb.stream != punktfunk_core::packet::V2_STREAM_VIDEO {
            if fb.keyframe {
                shared.screens.ask_keyframe(fb.stream);
            }
            return Ok(());
        }
        if fb.link_kbps != 0
            && shared.link_kbps.swap(fb.link_kbps, Ordering::Relaxed) != fb.link_kbps
        {
            tracing::info!(link_kbps = fb.link_kbps, "client's link rate");
        }
        if let Some((last, mask)) = fb.acked {
            shared
                .acked
                .store(u64::from(last) << 16 | u64::from(mask), Ordering::Relaxed);
        }
        let wake = apply_shape(&shared.shape, fb.shape);
        self.link.note_link(fb.link_kbps, wake);
        // The ask before the window: an RFI raised in a window counts in its report.
        if take_newer(u32::from(fb.ask), &mut self.last_ask, u16::MAX.into()) {
            self.on_recovery_ask(&fb)?;
        }
        if take_newer(fb.window, &mut self.last_window, u32::MAX) {
            self.on_report_window(&fb).await?;
        }
        Ok(())
    }

    /// The client's recovery ask: a keyframe, a reference-frame invalidation, or a resend.
    fn on_recovery_ask(&mut self, fb: &Feedback) -> Step {
        let task = &self.task;
        if fb.keyframe {
            // Encode loop coalesces: a wedge fires several requests before the IDR lands.
            tracing::debug!("client requested keyframe (decode recovery)");
            self.link.note_keyframe_req();
            task.ends.keyframe_tx.send(())?;
            poke_capture(&task.shared.arrival_poke);
        } else if let Some((first, last)) = fb.invalidate {
            // Encode loop falls back to a coalesced IDR when the range is too old or the
            // encoder has no RFI.
            tracing::debug!(
                first,
                last,
                "client requested reference-frame invalidation (loss recovery)"
            );
            self.unrecovered.rfi();
            self.link.note_rfi();
            task.ends.rfi_tx.send((first, last))?;
            poke_capture(&task.shared.arrival_poke);
        } else if let Some(n) = fb.nack {
            let queued = task
                .shared
                .resend
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|send| send(n.frame, n.shards().to_vec()));
            if !queued {
                tracing::debug!(frame = n.frame, "nack dropped: the send queue is full");
            }
        }
        Ok(())
    }

    /// A report window, newly closed: the offload guard, the loss and link notes, adaptive
    /// FEC, the delivery count, then the share of the path this session is on.
    async fn on_report_window(&mut self, fb: &Feedback) -> Step {
        // Before FEC: the send mode is not FEC's business, pinned or adaptive.
        if let Some(g) = self.offload_guard.as_mut() {
            g.on_report(fb.loss_ppm);
        }
        let unrecovered_run = self.unrecovered.report(std::time::Instant::now());
        let shared = &self.task.shared;
        if unrecovered_run > 0
            || punktfunk_core::abr::shows_loss_shape(fb.head, fb.mid, fb.tail, fb.sock_drops)
        {
            shared.lossy_window.store(true, Ordering::Relaxed);
        }
        self.link.note_loss(fb.loss_ppm, unrecovered_run);
        self.link
            .note_positions(fb.head, fb.mid, fb.tail, fb.sock_drops);
        self.link.sample_bands(
            shared.fec_target.load(Ordering::Relaxed),
            shared.live_bitrate.load(Ordering::Relaxed),
        );
        // No-op when FEC is pinned (`PUNKTFUNK_FEC_PCT`).
        if self.task.adaptive_fec {
            self.adapt_fec(fb, unrecovered_run);
        }
        // Unconditional: stall diagnosis needs `loss_ppm = 0` even when FEC is pinned or
        // adaptive FEC is off. Saturate into the u32 bridge; the value only matters near zero.
        self.task.shared.client_packets_received.store(
            fb.packets_received.min(u32::MAX as u64 - 1) as u32,
            Ordering::Relaxed,
        );
        let share = delivery_share(
            std::time::Instant::now(),
            fb.packets_received,
            &self.task.counters,
            &mut self.window,
            &mut self.share_clocks,
            self.task.bitrate_automatic,
            self.task.wire_bytes,
        );
        match share {
            Some(share) => self.on_share(share).await,
            None => Ok(()),
        }
    }

    /// Adaptive FEC's proposal for this window. It lands on `fec_requested`; the stream loop
    /// publishes it to `fec_target` (what the send loop reads per frame) once the encoder
    /// accepts the matching rate.
    fn adapt_fec(&mut self, fb: &Feedback, unrecovered_run: u32) {
        let shared = &self.task.shared;
        let prev = shared.fec_target.load(Ordering::Relaxed);
        let target = punktfunk_core::abr::budget::fec_target(
            fb.loss_ppm,
            prev,
            unrecovered_run,
            punktfunk_core::abr::budget::FrameBudget {
                budget_kbps: shared.live_bitrate.load(Ordering::Relaxed),
                audio_kbps: self.task.audio_kbps,
                shard_payload: self
                    .task
                    .wire_bytes
                    .saturating_sub(punktfunk_core::abr::budget::SHARD_WIRE_OVERHEAD)
                    .try_into()
                    .unwrap_or(u16::MAX),
                fps: self.active.refresh_hz,
            },
            &mut self.fec_horizon,
        );
        shared.fec_requested.store(target, Ordering::Release);
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

    /// The governor's share of the path, never under PyroWave's floor. Under the live rate it
    /// is a retarget the encoder takes now; above it, a ceiling the client still has to earn.
    /// A hand-back binds nothing: the path goes as a ceiling, and the release follows it.
    async fn on_share(&mut self, share: u32) -> Step {
        let share = share.max(self.pyrowave_pin.floor);
        let binds = self.task.counters.share.share_kbps() > 0;
        let live = self.task.shared.live_bitrate.load(Ordering::Relaxed);
        if binds && live > share {
            self.task.ends.bitrate_tx.send(share)?;
        }
        let ack = bitrate_ack(share, AckReason::Governor, self.task.ack_reason);
        self.send(&ack).await?;
        if !binds && self.task.ack_reason {
            let release = bitrate_ack(NO_SHARE_KBPS, AckReason::Governor, true);
            self.send(&release).await?;
        }
        Ok(())
    }

    async fn on_probe_result(&mut self, result: Option<ProbeResult>) -> Step {
        let result = result.ok_or(Ended)?;
        self.send(&result).await
    }

    /// The wire-MTU watcher's new shard payload, to the client.
    async fn on_shard_change(&mut self, n: Option<u16>) -> Step {
        let Some(n) = n else {
            self.closed.shard_change = true;
            return Ok(());
        };
        self.send(&punktfunk_core::quic::ShardPayloadChanged { shard_payload: n })
            .await
    }

    /// The encode loop's newest cursor shape; an error is the encode loop gone. A shape of
    /// ≤ ~58 KiB fits the u16 frame (`cursor_fwd` downscales).
    async fn on_cursor_shape(
        &mut self,
        changed: Result<(), tokio::sync::watch::error::RecvError>,
    ) -> Step {
        changed.map_err(|_| Ended)?;
        let shape = self.task.ends.cursor_shape_rx.borrow_and_update().clone();
        match shape {
            Some(shape) => self.send(&shape).await,
            None => Ok(()),
        }
    }

    async fn on_launch_outcome(
        &mut self,
        outcome: Option<punktfunk_core::quic::LaunchOutcome>,
    ) -> Step {
        let Some(outcome) = outcome else {
            self.closed.launch_outcome = true;
            return Ok(());
        };
        tracing::info!(
            outcome = outcome.kind.as_str(),
            said = %outcome.message,
            "told the client how its launch turned out"
        );
        self.send(&outcome).await
    }

    /// The operator's mute, so the client can name the silence rather than conceal a gap.
    async fn on_audio_state(&mut self, state: Option<punktfunk_core::quic::AudioState>) -> Step {
        let Some(state) = state else {
            self.closed.audio = true;
            return Ok(());
        };
        self.send(&state).await
    }

    async fn on_audio_config(&mut self, cfg: Option<v2msg::AudioConfig>) -> Step {
        let Some(cfg) = cfg else {
            self.closed.audio_config = true;
            return Ok(());
        };
        self.send(&cfg).await
    }

    async fn on_screen_tell(&mut self, tell: Option<super::wiring::ScreenTell>) -> Step {
        match tell {
            None => {
                self.closed.screen_tell = true;
                Ok(())
            }
            Some(super::wiring::ScreenTell::Config(cfg)) => self.send(&cfg).await,
            Some(super::wiring::ScreenTell::Layout(built)) => {
                tracing::info!(screens = ?built, "screen layout");
                self.send(&v2msg::ScreenLayout { screens: built }).await
            }
        }
    }

    async fn on_pad(&mut self, msg: Option<super::input::PadToClient>) -> Step {
        match msg {
            None => {
                self.closed.pad_slots = true;
                Ok(())
            }
            Some(super::input::PadToClient::Slots(s)) => self.send(&s).await,
            Some(super::input::PadToClient::Feature(f)) => self.send(&f).await,
        }
    }

    /// A grant change. Clearing `clip_enabled` only stops host→client: the selection this
    /// device installed stays on the host clipboard until `SetEnabled(false)` drops it.
    async fn on_access(&mut self, update: Option<punktfunk_core::quic::AccessUpdate>) -> Step {
        let Some(u) = update else {
            self.closed.access = true;
            return Ok(());
        };
        if u.grants & punktfunk_core::quic::GRANT_CLIPBOARD == 0 {
            let _ = self.task.clip.cmd_tx.send(ClipCoordCmd::SetEnabled(false));
        }
        self.send(&u).await
    }

    /// A host clipboard offer, forwarded only while sync is on: a race with a just-received
    /// disable would leak a stale offer.
    async fn on_host_clip_offer(&mut self, offer: Option<ClipOffer>) -> Step {
        let Some(offer) = offer else {
            self.closed.clip_offer = true;
            return Ok(());
        };
        if self.task.clip_enabled.load(Ordering::SeqCst) {
            self.send(&offer).await?;
        }
        Ok(())
    }

    /// The rate a pipeline rebuild settled on, as the `BitrateChanged` a `SetBitrate` gets: a
    /// mode switch re-resolves PyroWave's rate, and the live-rate display would otherwise
    /// stay on the old number.
    async fn on_retarget(&mut self, retarget: Option<(u32, AckReason)>) -> Step {
        let (kbps, why) = retarget.ok_or(Ended)?;
        // An explicit re-resolve is the pin itself moving: the verdict bound follows it, or a
        // queued mode switch would let a stale pin admit a raise. Automatic bounds follow the
        // delivered mode.
        if self.task.codec == crate::encode::Codec::PyroWave && !self.pyrowave_pin.automatic {
            self.pyrowave_pin.kbps = kbps;
        }
        tracing::info!(
            kbps,
            "encoder re-targeted by a pipeline rebuild — telling the client"
        );
        let ack = bitrate_ack(kbps, why, self.task.ack_reason);
        self.send(&ack).await
    }

    /// A rebuild's measured gap, after the fact: the client's bitrate controller drops the
    /// straddling report window rather than read the stall as congestion.
    async fn on_gap(&mut self, gap: Option<u32>) -> Step {
        let gap_ms = gap.ok_or(Ended)?;
        self.link.note_gap();
        tracing::info!(
            gap_ms,
            "pipeline rebuilt in place — telling the client the stream had a gap"
        );
        self.send(&PipelineGap { gap_ms }).await
    }

    /// Close the minute's window, log it, and append it to an armed capture. The FEC and ABR
    /// bands are sampled here as well as per report window, so the line names the rates a
    /// silent minute ran at rather than a zero it never measured.
    fn emit_link(&mut self) {
        let task = &self.task;
        let m = self.link.close(
            &task.counters.link,
            task.shared.fec_target.load(Ordering::Relaxed),
            task.shared.live_bitrate.load(Ordering::Relaxed),
        );
        crate::link_health::emit(&m, task.peer);
        if task.stats.is_armed() {
            task.stats.push_link(m);
        }
    }

    /// The stream thread's answer to a codec ask, acked with the ask it answers. A yes
    /// reached the client as the new epoch's config already.
    async fn on_format_result(&mut self, outcome: Option<super::wiring::FormatOutcome>) -> Step {
        let outcome = outcome.ok_or(Ended)?;
        let ack = Reconfigured {
            accepted: outcome.is_some(),
            mode: self.active,
            exclude_codecs: std::mem::take(&mut self.asked),
        };
        self.send(&ack).await
    }

    /// What an epoch delivers. Keeps `active` truthful for later refusal echoes, an
    /// Automatic PyroWave session's bounds on the mode it encodes, and the codec a mode is
    /// checked for; then tells the client what it does not hold.
    async fn on_delivered(&mut self, delivered: Option<super::wiring::Delivered>) -> Step {
        let d = delivered.ok_or(Ended)?;
        self.active = d.mode;
        self.task.codec = crate::encode::codec_from_wire(d.format.codec);
        if self.pyrowave_pin.automatic {
            (self.pyrowave_pin.floor, self.pyrowave_pin.kbps) = self.task.pyrowave_bounds(&d.mode);
        }
        match tell(&mut self.held_config, &d) {
            Some(Tell::Config(cfg)) => self.send(&cfg).await,
            Some(Tell::Correct(ack)) => self.send(&ack).await,
            None => Ok(()),
        }
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

/// Fire the stream loop's capture poke, so a recovery ask goes out this tick, not the next.
fn poke_capture(poke: &super::wiring::ArrivalPoke) {
    let poke = poke.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(poke) = poke {
        poke();
    }
}

/// Reported loss at which send offload is suspected of costing delivered throughput. 0.4 % is
/// what the 2026-07-14 Linux GSO A/B produced at a rate `sendmmsg` carried clean; under it the
/// offload is not the discriminator.
const OFFLOAD_LOSS_PPM: u32 = 4_000;
/// Windows over [`OFFLOAD_LOSS_PPM`] before the downshift (~1.5 s), so one Wi-Fi scan or BT
/// coexistence burst cannot strip the offload.
const OFFLOAD_LOSS_WINDOWS: u8 = 2;

/// One-way guard on UDP send offload (Linux GSO, Windows USO). The offload hands the kernel
/// super-buffers that leave at NIC line rate; where the path's bottleneck sits below that,
/// the train overruns a queue the one-packet sends did not. Nothing on the host predicts that
/// path, so the client's own loss reports decide, and the verdict is sticky for the session:
/// the send mode never oscillates mid-stream.
struct OffloadGuard {
    /// Cleared to downshift; the media sender reads it per send.
    switch: std::sync::Arc<std::sync::atomic::AtomicBool>,
    over: u8,
}

impl OffloadGuard {
    /// One report window. Built only when the offload is not pinned by `PUNKTFUNK_GSO`.
    fn on_report(&mut self, loss_ppm: u32) {
        if loss_ppm < OFFLOAD_LOSS_PPM {
            self.over = 0;
            return;
        }
        self.over = self.over.saturating_add(1);
        // `swap` is the once-only latch: only the transition logs.
        if self.over >= OFFLOAD_LOSS_WINDOWS
            && self
                .switch
                .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                loss_ppm,
                windows = self.over,
                "UDP send offload looks to be costing delivered throughput on this path — \
                 every packet leaves on its own for the rest of the session (PUNKTFUNK_GSO=1 \
                 pins it on)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::ProbeRequest;

    #[test]
    fn offload_guard_downshifts_only_on_sustained_loss() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mk = || {
            let switch = std::sync::Arc::new(AtomicBool::new(true));
            (
                OffloadGuard {
                    switch: switch.clone(),
                    over: 0,
                },
                switch,
            )
        };
        // Loss under the threshold never downshifts, however long it runs.
        let (mut g, sw) = mk();
        for _ in 0..100 {
            g.on_report(OFFLOAD_LOSS_PPM - 1);
        }
        assert!(sw.load(Ordering::Relaxed));
        // One window over is not enough; the second takes the offload away.
        let (mut g, sw) = mk();
        g.on_report(OFFLOAD_LOSS_PPM);
        assert!(sw.load(Ordering::Relaxed));
        g.on_report(OFFLOAD_LOSS_PPM);
        assert!(!sw.load(Ordering::Relaxed));
        // A clean window between two bad ones resets the run: an isolated burst is no verdict.
        let (mut g, sw) = mk();
        g.on_report(OFFLOAD_LOSS_PPM);
        g.on_report(0);
        g.on_report(OFFLOAD_LOSS_PPM);
        assert!(sw.load(Ordering::Relaxed));
        g.on_report(OFFLOAD_LOSS_PPM);
        assert!(!sw.load(Ordering::Relaxed));
    }
    use punktfunk_core::quic::{
        CLIP_FLAG_FILES, CLIP_POLICY_FILES, CLIP_POLICY_TEXT, CLIP_REASON_BACKEND_UNAVAILABLE,
        CLIP_REASON_NOT_PERMITTED, CLIP_REASON_NO_FILES, CLIP_REASON_OK,
        CLIP_REASON_POLICY_DISABLED, GRANT_ALL, GRANT_CLIPBOARD,
    };

    /// A probe into a 1 GbE port is served at 900 Mbit/s at most; unknown ports bound nothing.
    #[test]
    fn a_probe_never_asks_more_than_the_ports_carry() {
        let eth = punktfunk_core::transport::IFACE_KIND_ETHERNET;
        let gbe = crate::send_pacing::Ports::of((eth, 2_500), (eth, 1_000));
        assert_eq!(probe_bound(1_200_000, gbe), 900_000);
        assert_eq!(probe_bound(50_000, gbe), 50_000);
        assert_eq!(probe_bound(1_200_000, Default::default()), 1_200_000);
    }

    /// A client gets the wake shape or the default, never the operator's line rate.
    #[test]
    fn a_client_asks_for_the_wake_shape_or_nothing() {
        let slot = std::sync::atomic::AtomicU8::new(0);
        assert!(apply_shape(&slot, 1));
        assert!(!apply_shape(&slot, 2));
        assert_eq!(
            slot.load(Ordering::Relaxed),
            0,
            "burst is not the client's to ask"
        );
    }

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
            format: Default::default(),
            epoch,
            corrects,
        };
        let mut held = Some(v2msg::StreamConfig {
            epoch: 0,
            mode: mode(1280),
            ..Default::default()
        });
        held.as_mut().unwrap().set_format(Default::default());
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
                mode: mode(800),
                exclude_codecs: 0,
            }))
        );
    }

    /// A format change is told even at the same epoch and mode; the same format twice is not.
    #[test]
    fn a_new_format_is_a_config() {
        use super::super::wiring::Delivered;
        use punktfunk_core::quic::{VideoFormat, CODEC_H264, CODEC_HEVC};
        let d = |codec| Delivered {
            mode: Default::default(),
            format: VideoFormat {
                codec,
                ..VideoFormat::default()
            },
            epoch: 3,
            corrects: false,
        };
        let mut held = Some(v2msg::StreamConfig {
            epoch: 3,
            codec: CODEC_HEVC,
            ..Default::default()
        });
        let Some(Tell::Config(cfg)) = tell(&mut held, &d(CODEC_H264)) else {
            panic!("a new format is configured")
        };
        assert_eq!(cfg.codec, CODEC_H264);
        assert_eq!(tell(&mut held, &d(CODEC_H264)), None, "already held");
    }

    /// A codec ask takes the client's preference among what it has left, never PyroWave, and
    /// never off PyroWave.
    #[test]
    fn a_codec_ask_moves_to_the_preferred_codec_left() {
        use crate::encode::Codec;
        use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE};
        let all = CODEC_H264 | CODEC_HEVC | CODEC_AV1 | CODEC_PYROWAVE;
        let left = CODEC_H264 | CODEC_HEVC | CODEC_PYROWAVE;
        assert_eq!(
            next_codec(Codec::Av1, left, all, CODEC_HEVC),
            Some(Codec::H265)
        );
        assert_eq!(
            next_codec(Codec::Av1, left, all, CODEC_PYROWAVE),
            Some(Codec::H265),
            "a PyroWave preference is skipped"
        );
        assert_eq!(
            next_codec(Codec::H265, CODEC_H264, all, 0),
            Some(Codec::H264)
        );
        assert_eq!(
            next_codec(Codec::H264, CODEC_H264, all, 0),
            None,
            "nothing new left"
        );
        assert_eq!(
            next_codec(Codec::H264, 0, all, 0),
            None,
            "nothing left at all"
        );
        assert_eq!(
            next_codec(Codec::H265, CODEC_AV1, CODEC_H264, 0),
            None,
            "host can't"
        );
        assert_eq!(next_codec(Codec::PyroWave, CODEC_H264, all, 0), None);
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
                    fixed_kbps: None,
                    floor_kbps: None,
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

    /// A client whose `ClientHello` set no `abr_features` gets an ack without a reason,
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
