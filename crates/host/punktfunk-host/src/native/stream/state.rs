//! The virtual-display stream loop's state. [`StreamState::new`] is bring-up (display,
//! pipeline, launch, game lease, send thread); [`StreamState::run`] is the tick loop, one
//! method per concern:
//!
//! - `rebuild.rs`: session switch, mode switch, topology re-assert, capture loss, source-mode
//!   follow — everything that swaps the pipeline under the loop.
//! - `swap.rs`: the swap itself, and what restarts with an encoder.
//! - `recovery.rs`: FEC/bitrate re-derivation and the keyframe/RFI coalescing.
//! - `encode.rs`: capture tick, submit, poll, stall watch, depth adaptation, pacing sleep.
//! - `cursor.rs`: cursor forwarding, host composite, the seat-pointer park.
//! - `resize.rs`: the Windows in-place resize.
//!
//! Fields group by lifetime: `facts`, `shared` and `ends` (`facts.rs`) are fixed for the session,
//! `live` is what a swap replaces, `display` what a rebuild reopens with, `diag` only logs, and
//! the rest is the loop's own bookkeeping. Field order is teardown order. A per-tick value
//! (`repeat`, `t_cap`, `owed`) travels in [`Tick`], not here.

use super::cursor::composite_plan;
#[cfg(target_os = "linux")]
use super::cursor::settle_portal_cursor;
use super::facts::{LoopEnds, SessionFacts, Shared};
use super::pipeline::{build_pipeline_with_retry, resolve_plan, BuildRequest, Pipeline};
use super::*;
use crate::native::bitrate::{pyrowave_mode_kbps, EncDerive};
use crate::session_status::pack_mode;

/// (capture_ns, submit_ns) per frame handed to the encoder and not yet polled.
pub(super) type Inflight = std::collections::VecDeque<(u64, u64)>;

/// What one tick's capture phase hands to its encode phase.
#[derive(Clone, Copy)]
pub(super) struct Tick {
    pub(super) t_cap: std::time::Instant,
    pub(super) cap_us: u32,
    /// `try_latest` had nothing: the previous frame is re-encoded.
    pub(super) repeat: bool,
    pub(super) measure: bool,
}

/// After the encode phase: `Next` runs the tail of the tick, the others skip it.
pub(super) enum Flow {
    Next,
    Continue,
    Break,
}

/// What the wire is owed and when the encoder last answered.
pub(super) struct AuClock {
    /// Predicted as `au_seq + inflight.len()`. Encoder-internal counters desync on the first ABR rebuild.
    pub(super) au_seq: u32,
    /// A chunked AU's FIRST went out and its LAST has not: `au_seq` still names that frame.
    pub(super) wire_frame_open: bool,
    pub(super) inflight: Inflight,
    pub(super) watchdog: crate::encode_recovery::EncoderWatchdog,
    /// The last IDR the loop forced or opened on. The keyframe cooldown counts from it.
    pub(super) last_forced_idr: Option<std::time::Instant>,
}

impl AuClock {
    /// The clock of a pipeline that opened on an IDR at `now`: nothing owed, and the cold-GOP
    /// keyframe asks coalesce into that IDR.
    fn opened(now: std::time::Instant) -> Self {
        AuClock {
            au_seq: 0,
            wire_frame_open: false,
            inflight: Inflight::new(),
            watchdog: crate::encode_recovery::EncoderWatchdog::new(),
            last_forced_idr: Some(now),
        }
    }

    /// The wire index a replacement encoder's first AU takes: past a frame still open on the
    /// wire, which closes when that encoder takes over.
    pub(super) fn next_au(&self) -> u32 {
        self.au_seq.wrapping_add(u32::from(self.wire_frame_open))
    }

    /// The encoder was rebuilt in place, as a swap rebuilds one: what it owed is gone, a frame
    /// whose tail never came is closed on the wire (the next AU is a new frame, or the client
    /// sees the off-by-one `close_open_frame` exists for), and its forced IDR starts the
    /// keyframe cooldown, so the client's echo of the same freeze forces no second one.
    pub(super) fn after_reset(&mut self) {
        self.inflight.clear();
        if std::mem::take(&mut self.wire_frame_open) {
            self.au_seq = self.au_seq.wrapping_add(1);
        }
        self.last_forced_idr = Some(std::time::Instant::now());
    }
}

/// The escalation `adapt_to_cadence` makes when encode cannot hold cadence: the encoder's
/// pipelined retrieve, wound back after a clean run.
pub(super) struct Pipelining {
    /// `PUNKTFUNK_IDD_ADAPTIVE`: off, `adapt_to_cadence` does nothing.
    pub(super) adaptive: bool,
    pub(super) behind_score: u32,
    pub(super) depth_frames: u64,
    pub(super) pipeline_asked: bool,
    pub(super) pipelined_active: bool,
    pub(super) deescalating: bool,
    pub(super) ahead_run: u32,
    pub(super) deescalate_not_before: Option<std::time::Instant>,
    pub(super) deescalate_backoff: std::time::Duration,
}

impl Pipelining {
    /// Nothing escalated yet.
    fn new(adaptive: bool) -> Self {
        Pipelining {
            adaptive,
            behind_score: 0,
            depth_frames: 0,
            pipeline_asked: false,
            pipelined_active: false,
            deescalating: false,
            ahead_run: 0,
            deescalate_not_before: None,
            deescalate_backoff: super::encode::DEESCALATE_BACKOFF_START,
        }
    }
}

/// Adaptive FEC as the encode loop holds it: the control task proposes, and parity is applied
/// behind the encoder rate it leaves room for.
pub(super) struct FecState {
    /// The control task's proposal.
    pub(super) requested: Arc<AtomicU8>,
    /// Applied FEC: what the packetizer and [`StreamState::enc_now`] run at.
    pub(super) target: Arc<AtomicU8>,
    /// `target` as this loop last published it.
    pub(super) last: u8,
    /// A proposal an asynchronous encoder was asked to make room for: the parity and the
    /// encoder rate it implies, kbps. Parity waits for the encoder to settle.
    pub(super) pending: Option<(u8, u32)>,
    /// The proposal a hold was last logged for, so one the control task re-proposes every
    /// window logs once.
    pub(super) hold_logged: u8,
}

/// What the loop only logs: the capture split and stage timings under `PUNKTFUNK_PERF`, the
/// host-composite outcomes, and the cadence verdict's flips with the chains behind them.
pub(super) struct Diag {
    /// NEW source frames / REPEATS / cursor REGENS since `diag_at`. Logged every 2 s under `PUNKTFUNK_PERF`.
    pub(super) diag_new: u64,
    pub(super) diag_repeat: u64,
    pub(super) diag_regen: u64,
    pub(super) diag_at: std::time::Instant,
    /// Each host-composite outcome is logged once.
    #[cfg_attr(windows, allow(dead_code, reason = "Windows never composites"))]
    pub(super) composite_log: super::cursor::CompositeLog,
    pub(super) st_cap: Vec<u32>,
    pub(super) st_submit: Vec<u32>,
    pub(super) st_wait: Vec<u32>,
    pub(super) st_queue: Vec<u32>,
    /// The first degraded verdict logs the stage histograms once; until then they sample
    /// without `PUNKTFUNK_PERF` too.
    pub(super) stages_owed: bool,
    pub(super) was_degraded: bool,
    pub(super) last_cadence_log: Option<std::time::Instant>,
    pub(super) cadence_flips_suppressed: u32,
    pub(super) chain: super::encode::ChainWindow,
}

impl Diag {
    pub(super) fn new(now: std::time::Instant) -> Self {
        Diag {
            diag_new: 0,
            diag_repeat: 0,
            diag_regen: 0,
            diag_at: now,
            composite_log: Default::default(),
            st_cap: Vec::new(),
            st_submit: Vec::new(),
            st_wait: Vec::new(),
            st_queue: Vec::new(),
            stages_owed: true,
            was_degraded: false,
            last_cadence_log: None,
            cadence_flips_suppressed: 0,
            chain: Default::default(),
        }
    }
}

/// The running pipeline and what a swap replaces with it. Field order is drop order: the
/// capturer goes before the encoder, as on every swap.
pub(super) struct Live {
    pub(super) capturer: Box<dyn crate::capture::Capturer>,
    pub(super) enc: Box<dyn crate::encode::Encoder>,
    pub(super) frame: crate::capture::CapturedFrame,
    pub(super) interval: std::time::Duration,
    pub(super) cur_node_id: u32,
    pub(super) cur_display_gen: Option<u64>,
    /// The `punktfunk/2` stream epoch every AU from now on carries. It moves whenever the
    /// pipeline or the mode under it changes, so a client drops what was encoded before. A
    /// rate rebuild keeps it: the same picture at the same mode.
    pub(super) epoch: u8,
    /// The live output's metadata for a capture-only rebuild (`on_capture_lost`).
    #[cfg(target_os = "linux")]
    pub(super) lease: Option<crate::capture::OutputLease>,
    /// Source stamps moved off a display faster than the stream onto the content's cadence.
    /// `None` at 1x. The Windows driver corrects its own stamps.
    #[cfg(target_os = "linux")]
    pub(super) restamp: Option<super::encode::SourceStamps>,
    /// Source can change format/size with no client Reconfigure; in-place encoder reset cannot follow.
    pub(super) enc_src: (pf_frame::PixelFormat, u32, u32),
    /// The mode a rebuild reopens at: the client's latest ask, or the source's delivered size.
    pub(super) cur_mode: punktfunk_core::Mode,
    /// Total wire budget (kbps). Only encoder opens convert via [`EncDerive`].
    pub(super) bitrate_kbps: u32,
    /// A rate was handed to this encoder after it opened, so a later read-back
    /// is about that retarget. What a pipeline opened at is the build's own
    /// business (`Pipeline::bitrate_kbps`), and is not re-litigated here.
    pub(super) retargeted: bool,
    /// What the capturer raised and this loop has not answered: the parked rung after the
    /// grab, the outage and the torn reference with the next recovery asks.
    pub(super) asks: crate::encode_recovery::SourceAsks,
    /// The first screen's share of the video, as its encoder last took it.
    pub(super) applied_share: u16,
}

/// The display this session streams, the request its displays open with, and the cursor plan
/// that follows the compositor. A rebuild that moves the compositor, the route or the HDR
/// verdict writes it here. `vd` drops after the pipeline that captures it.
pub(super) struct DisplayRequest {
    pub(super) vd: Box<dyn crate::vdisplay::VirtualDisplay>,
    pub(super) compositor: crate::vdisplay::Compositor,
    pub(super) cursor_fwd: Option<super::super::cursor_fwd::CursorForwarder>,
    /// Starts true so the first composite request triggers the capturer hook.
    pub(super) cursor_client_drew: bool,
    pub(super) gamescope_composite: bool,
    pub(super) metadata_composite: bool,
    #[cfg(target_os = "linux")]
    pub(super) no_overlay_means_off_output: bool,
    /// What every display of this session opens with. Bring-up's launch rides it; a rebuild
    /// opens without it.
    pub(super) vd_params: crate::vdisplay::SessionParams,
    /// The seat-pointer park schedule, per (re)built display.
    #[cfg(target_os = "linux")]
    pub(super) parked_display: Option<(u32, Option<u64>)>,
    #[cfg(target_os = "linux")]
    pub(super) park_attempts: u32,
    #[cfg(target_os = "linux")]
    pub(super) next_park_at: std::time::Instant,
    /// `SessionCounters::pointer_moves` when this display's first frame armed the schedule.
    #[cfg(target_os = "linux")]
    pub(super) moves_at_arm: u64,
}

pub(super) struct StreamState {
    // ---- loop bookkeeping ----
    pub(super) next: std::time::Instant,
    pub(super) sent: u64,
    /// Survives in-loop rebuilds: a rebuild must not reopen the overshoot.
    pub(super) pace: CaptureCredit,
    pub(super) clock: AuClock,
    pub(super) capture_rebuilds: u32,
    /// Topology re-assert generation this loop last saw. Only Windows IDD-push moves it.
    pub(super) seen_reassert_gen: u64,
    pub(super) last_hdr_meta: Option<pf_frame::HdrMeta>,
    /// `last_hdr_meta` changed and its datagram has not gone out: it rides the next AU,
    /// whichever tick polls one.
    pub(super) hdr_meta_owed: bool,
    pub(super) diag: Diag,
    /// Never re-anchors the IDR cooldown: sustained loss + RFI would swallow IDR pleas forever.
    pub(super) last_rfi: Option<std::time::Instant>,
    /// A host-origin repair IDR (a torn reference, an operator's force) waiting out the base
    /// cooldown after the last IDR. It never rides the client's echo gate.
    pub(super) owed_idr: bool,
    pub(super) loss_mode: super::recovery::LossMode,
    pub(super) kf_gate: super::recovery::KeyframeGate,
    pub(super) recovery_cadence: pf_frame::metronome::Metronome,
    pub(super) ir_wave_pos: u32,
    pub(super) pipelining: Pipelining,
    pub(super) fec: FecState,
    /// EMA of real-frame arrivals. Negotiated refresh is the wrong deadline when the game is slower.
    pub(super) src_period_ns: Option<u64>,
    /// Longest host submit+poll chain observed this tick, independent of perf sampling.
    pub(super) encode_chain_ns: u64,
    pub(super) last_real_cap: Option<std::time::Instant>,

    // ---- teardown order, which is field order: the send thread first, so the summary the
    // registration emits holds its last counters; then game policy, capture, encoder, display ----
    pub(super) _watcher: Option<std::thread::JoinHandle<()>>,
    /// `Some(joined)` on the seat's own session, which follows the seat's mode.
    #[cfg(target_os = "linux")]
    pub(super) seat_join: Option<bool>,
    /// The session's place among the seat's followers, left when it ends.
    #[cfg(target_os = "linux")]
    pub(super) following: Option<crate::seats::session_switch::Following>,
    /// Screens past the first, when the session negotiated them. Before `send`: their loops
    /// hold senders the send thread waits out.
    pub(super) screens: Option<super::screens::ScreenSet>,
    pub(super) send: super::send::SendThread,
    pub(super) live_session: crate::session_status::LiveSessionGuard,
    pub(super) health_published_at: std::time::Instant,
    pub(super) game_life: Option<crate::gamelease::SessionGuard>,
    pub(super) live: Live,
    pub(super) display: DisplayRequest,

    // ---- for the session ----
    /// What the session encodes: a codec change re-resolves both, a compositor change the
    /// plan's cursor fields, each before it builds.
    pub(super) plan: crate::session_plan::SessionPlan,
    pub(super) bit_depth: u8,
    pub(super) facts: SessionFacts,
    pub(super) shared: Shared,
    pub(super) ends: LoopEnds,
}

/// The video format `plan` encodes at `bit_depth` over `capturer` now, as a `StreamConfig` names
/// it: an HDR session whose source composes SDR is 8-bit BT.709 (`session_plan::live_depth`).
pub(super) fn format_of(
    plan: &crate::session_plan::SessionPlan,
    capturer: &dyn crate::capture::Capturer,
    bit_depth: u8,
) -> punktfunk_core::quic::VideoFormat {
    let (hdr, bit_depth) =
        crate::session_plan::live_depth(plan.hdr, capturer.colour().hdr_meta.is_some(), bit_depth);
    let color = punktfunk_core::quic::ColorInfo::for_session(hdr, plan.full_range(hdr, bit_depth));
    video_format(plan.codec, bit_depth, color, plan.chroma)
}

/// The format as a `StreamConfig` names it. `color` is `ColorInfo::for_session`'s: the HDR
/// verdict at the plan's range.
pub(in crate::native) fn video_format(
    codec: crate::encode::Codec,
    bit_depth: u8,
    color: punktfunk_core::quic::ColorInfo,
    chroma: crate::encode::ChromaFormat,
) -> punktfunk_core::quic::VideoFormat {
    punktfunk_core::quic::VideoFormat {
        codec: crate::encode::codec_to_wire(codec),
        bit_depth,
        color,
        chroma_format: crate::encode::chroma_idc(chroma),
    }
}

impl StreamState {
    /// The encoder-rate derivation for a FEC percentage.
    pub(super) fn enc_derive(&self, fec: u8) -> EncDerive {
        EncDerive {
            audio_kbps: self.facts.audio_reserved_kbps,
            shard_payload: self.facts.shard_payload,
            fec_percent: fec,
            head_only_hz: self.facts.pyrowave.then_some(self.live.cur_mode.refresh_hz),
            share_permille: self
                .shared
                .screen_wire
                .primary_share
                .load(Ordering::Relaxed),
        }
    }

    /// Follow the further screens: the client's list, their layout, and the first screen's
    /// share of the video, which its encoder retargets to in place.
    pub(super) fn on_screens(&mut self) {
        let (mode, first_gen) = (self.delivered_mode(), self.live.cur_display_gen);
        if let Some(set) = self.screens.as_mut() {
            set.poll(mode, first_gen);
        }
        let share = self
            .shared
            .screen_wire
            .primary_share
            .load(Ordering::Relaxed);
        if share == self.live.applied_share {
            return;
        }
        self.live.applied_share = share;
        let kbps = self.enc_now().enc_kbps(self.live.bitrate_kbps);
        if self.live.enc.reconfigure_bitrate(u64::from(kbps) * 1000) {
            tracing::info!(
                share_permille = share,
                encoder_kbps = kbps,
                "first screen's share of the video moved"
            );
        } else {
            tracing::info!(
                share_permille = share,
                "first screen's share moves at its next rate change"
            );
        }
    }

    /// The further screens `asked` names, on threads of their own once the send thread takes
    /// frames. They open on the first screen's display request, plan and depth, at the whole
    /// video's derivation until the set moves the first screen's share.
    fn screen_set(
        &self,
        asked: Vec<punktfunk_core::quic::v2::msg::Screen>,
        rx: std::sync::mpsc::Receiver<Vec<punktfunk_core::quic::v2::msg::Screen>>,
        tell: tokio::sync::mpsc::UnboundedSender<crate::native::wiring::ScreenTell>,
        send: Arc<std::sync::mpsc::SyncSender<SendMsg>>,
    ) -> super::screens::ScreenSet {
        let base = super::screens::ScreenBase {
            compositor: self.display.compositor,
            params: self.display.vd_params.clone(),
            plan: self.plan,
            pyrowave_max_kbps: self.facts.knobs.pyrowave_max_kbps,
            encoder: self.facts.encoder.clone(),
            bit_depth: self.bit_depth,
            enc_of: EncDerive {
                share_permille: crate::native::bitrate::SHARE_ALL,
                ..self.enc_now()
            },
            fec_target: self.fec.target.clone(),
            live_bitrate: self.shared.live_bitrate.clone(),
            send,
            tell,
            wire: self.shared.screen_wire.clone(),
            life: self.facts.life.clone(),
            first: Arc::default(),
        };
        super::screens::ScreenSet::new(base, rx, asked)
    }

    /// Further screens open on the session's plan and display request: after either moved,
    /// restart them on the new ones.
    pub(super) fn rebase_screens(&mut self) {
        let (compositor, params, plan, bit_depth) = (
            self.display.compositor,
            self.display.vd_params.clone(),
            self.plan,
            self.bit_depth,
        );
        let enc_of = EncDerive {
            share_permille: crate::native::bitrate::SHARE_ALL,
            ..self.enc_now()
        };
        if let Some(set) = self.screens.as_mut() {
            set.rebase(|b| {
                (b.compositor, b.params, b.plan, b.bit_depth, b.enc_of) =
                    (compositor, params, plan, bit_depth, enc_of);
            });
        }
    }

    /// [`Self::enc_derive`] at the FEC target in force now.
    pub(super) fn enc_now(&self) -> EncDerive {
        self.enc_derive(self.fec.target.load(Ordering::Relaxed))
    }

    /// A rebuild's [`BuildRequest`] at `mode` and `bitrate_kbps`: this session's plan, encode
    /// resolution, depth and HDR metadata, the derivation in force now at `mode`'s refresh (a
    /// PyroWave frame's head parity is per frame), and the wire index its encoder continues at.
    pub(super) fn build_request(
        &self,
        mode: punktfunk_core::Mode,
        bitrate_kbps: u32,
    ) -> BuildRequest {
        BuildRequest {
            mode,
            bitrate_kbps,
            bitrate_auto: self.facts.bitrate_auto,
            bit_depth: self.bit_depth,
            enc_of: EncDerive {
                head_only_hz: self.facts.pyrowave.then_some(mode.refresh_hz),
                ..self.enc_now()
            },
            plan: self.plan,
            pyrowave_max_kbps: self.facts.knobs.pyrowave_max_kbps,
            encoder: self.facts.encoder.clone(),
            client_hdr: self.facts.client_hdr,
            wire_seq_base: self.clock.next_au(),
            screen: 0,
        }
    }

    /// Close a frame whose tail never came, as the loop does at the next first chunk, before a
    /// replacement encoder opens at `au_seq`. Left to the loop, that close would land after the
    /// new encoder numbered its first AU, one behind the wire for the rest of the session.
    pub(super) fn close_open_frame(&mut self) {
        if std::mem::take(&mut self.clock.wire_frame_open) {
            if self.clock.inflight.len() > 1 {
                self.clock.inflight.pop_front();
            }
            self.clock.au_seq = self.clock.au_seq.wrapping_add(1);
        }
    }

    pub(super) fn adopt_reframe(&self, reframe: punktfunk_core::video_fit::Reframe) {
        *self
            .shared
            .frame_map
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = reframe;
    }

    /// A rebuild re-resolved what it encodes. Noted for the summary's bitrate span: the
    /// rate the session runs at moved, whoever decided it.
    pub(super) fn adopt_built_bitrate(&mut self, built: u32) {
        if built != self.live.bitrate_kbps {
            self.facts.counters.note_bitrate(built);
        }
        adopt_built_bitrate(
            &mut self.live.bitrate_kbps,
            built,
            &self.shared.live_bitrate,
            &self.ends.retarget_tx,
        );
    }

    pub(super) fn delivered_mode(&self) -> punktfunk_core::Mode {
        delivered_mode(
            self.live.frame.width,
            self.live.frame.height,
            self.live.interval,
        )
    }

    /// Publish the delivered mode to `/status` and to the client, correcting it when it
    /// differs from `asked`.
    pub(super) fn publish_delivered_mode(&self, asked: punktfunk_core::Mode) {
        let actual = self.delivered_mode();
        self.shared.live_mode.store(
            pack_mode(actual.width, actual.height, actual.refresh_hz),
            Ordering::Relaxed,
        );
        self.tell_delivered(actual, actual != asked);
    }

    /// Tell the control task what the current epoch delivers, and `/status` what it encodes.
    pub(super) fn tell_delivered(&self, mode: punktfunk_core::Mode, corrects: bool) {
        let format = format_of(&self.plan, &*self.live.capturer, self.bit_depth);
        crate::session_status::set_format(
            self.live_session.id,
            self.plan.codec,
            format.color.is_hdr(),
            format.bit_depth,
            self.plan.chroma,
        );
        let _ = self
            .ends
            .reconfig_result_tx
            .send(crate::native::wiring::Delivered {
                mode,
                format,
                epoch: self.live.epoch,
                corrects,
            });
    }

    /// Bring the session up in phases. What the context settles comes first: the plan, the
    /// display and wire requests, a mode queued meanwhile, the facts and the shared handles.
    /// Then the bring-up ramp, the display and its first pipeline, the launch and game lease,
    /// the send thread, the registration, and the further screens on the assembled state. A
    /// phase's result is a local until the struct is assembled, so one that fails drops what the
    /// phases before it started: the lease, the pipeline, the display, the ramp.
    pub(super) fn new(ctx: SessionContext, prepared: Option<PreparedDisplay>) -> Result<Self> {
        let plan = session_plan(&ctx);
        let (cursor, vd_params) = display_request(&ctx, &plan);
        let (streamed_wire, slice_wire) = wire_request(&ctx);
        let (mode, bitrate_kbps, adopted) = settle_mode(&ctx, &plan, prepared.is_some());
        let mut facts = session_facts(&ctx, &plan, mode, streamed_wire);
        let (shared, fec) = shared_handles(&ctx);
        let c = ctx.common;
        // The data plane is bound and idle until the send thread starts. Answer the client's
        // bring-up ramp on it meanwhile: it measures the link with no video to damage.
        let ramp = ramp::RampServer::start(
            c.session,
            c.ends.probe_rx,
            c.ends.probe_result_tx.clone(),
            c.probe_seq,
            facts.life.stop_flag().clone(),
            c.shared.ramp_open.clone(),
            // Explicit-rate PyroWave: the client's ramp closes with one lower pin, so the
            // window lingers past pipeline-ready for it to cross.
            !facts.bitrate_auto && c.codec == crate::encode::Codec::PyroWave,
        );
        let (mut display, pipe) = open_display(prepared, ctx.compositor, vd_params, cursor)?;
        let pipe = match pipe {
            Some(pipe) => pipe,
            None => build_first(&mut display, &facts, plan, c.bit_depth, bitrate_kbps, &fec)?,
        };
        let live = first_live(
            pipe,
            &mut display,
            &shared,
            mode,
            bitrate_kbps,
            &c.ends.retarget_tx,
        );
        if adopted {
            tell_adopted(&c.ends.reconfig_result_tx, &live, &plan, c.bit_depth);
        }
        let (game_shared, game_life) = spawn_launch_and_lease(
            &display,
            live.cur_display_gen,
            &facts,
            ctx.launch_target.as_ref(),
            &ctx.launch_owner,
            ctx.launch_claim,
            ctx.launch_stamp,
            &ctx.launch_outcome,
        );
        let (send, frame_tx) = start_send(
            ramp,
            &facts,
            &shared,
            &c.shared,
            c.ends.probe_result_tx,
            c.ends.shard_rx,
            c.timing_conn,
            c.probe_seq,
            c.wire_sock,
            &c.client_label,
            plan.codec,
            slice_wire,
        )?;
        let live_session = register_session(
            &facts,
            &shared,
            &plan,
            c.bit_depth,
            ctx.join_live,
            c.client_label,
            c.client_name,
            c.controls,
            game_shared,
        );
        let now = std::time::Instant::now();
        // The `seconds` cap counts from a stream that is up.
        facts.deadline = now + std::time::Duration::from_secs(u64::from(c.seconds));
        let kf_gate = super::recovery::KeyframeGate::new(c.shared.refuted.clone());
        let mut st = Self {
            plan,
            facts,
            bit_depth: c.bit_depth,
            display,
            shared,
            live,
            ends: LoopEnds {
                cursor_shape_tx: c.ends.cursor_shape_tx,
                #[cfg(target_os = "linux")]
                input_tx: ctx.input_tx,
                #[cfg(target_os = "linux")]
                input_route: ctx.input_route,
                #[cfg(target_os = "linux")]
                inj_shared_tx: ctx.inj_shared_tx,
                #[cfg(target_os = "linux")]
                inj_session_tx: ctx.inj_session_tx,
                reconfig: c.ends.reconfig,
                keyframe: c.ends.keyframe,
                rfi: c.ends.rfi,
                bitrate_rx: c.ends.bitrate_rx,
                // Disconnected until `spawn_session_watcher` replaces it inside the session span.
                session_rx: std::sync::mpsc::channel().1,
                reconfig_result_tx: c.ends.reconfig_result_tx,
                retarget_tx: c.ends.retarget_tx,
                gap_tx: c.ends.gap_tx,
                format_rx: c.ends.format_rx,
                format_result_tx: c.ends.format_result_tx,
            },
            screens: None,
            send,
            health_published_at: now,
            next: now,
            sent: 0,
            pace: CaptureCredit::new(now),
            clock: AuClock::opened(now),
            capture_rebuilds: 0,
            seen_reassert_gen: crate::windows::idd::topology_reassert_gen(),
            last_hdr_meta: None,
            hdr_meta_owed: false,
            diag: Diag::new(now),
            last_rfi: None,
            owed_idr: false,
            loss_mode: super::recovery::LossMode::new(ctx.host_knobs.acked_refs),
            kf_gate,
            recovery_cadence: pf_frame::metronome::Metronome::new(),
            ir_wave_pos: 0,
            pipelining: Pipelining::new(ctx.host_knobs.idd_adaptive),
            fec,
            src_period_ns: None,
            encode_chain_ns: 0,
            last_real_cap: None,
            live_session,
            _watcher: None,
            #[cfg(target_os = "linux")]
            seat_join: ctx.follows_seat.then_some(ctx.join_live),
            #[cfg(target_os = "linux")]
            following: None,
            game_life,
        };
        let (rx, tell) = (c.ends.screens_rx, c.ends.screen_tell_tx);
        st.screens = ctx
            .screens
            .map(|asked| st.screen_set(asked, rx, tell, frame_tx));
        Ok(st)
    }

    /// Follow a mid-stream Gaming↔Desktop switch: the seat's own, else the box's when the
    /// session's `PUNKTFUNK_SESSION_WATCH` allows and `PUNKTFUNK_COMPOSITOR` does not pin the
    /// backend. A device joined to a seat's screen ends instead; its reconnect joins the new one.
    fn spawn_session_watcher(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(joined) = self.seat_join {
            let (session_tx, session_rx) = std::sync::mpsc::channel::<SessionSwitch>();
            self.ends.session_rx = session_rx;
            let life = self.facts.life.clone();
            let catch_up = session_tx.clone();
            let (following, game) = crate::seats::session_switch::follow(Box::new(move |game| {
                if joined {
                    life.end(crate::events::SessionEndReason::HostEnded);
                    return false;
                }
                session_tx.send(SessionSwitch::seat(game)).is_ok()
            }));
            self.following = Some(following);
            // A switch while this session was starting went by unheard.
            let built_in_game = self.display.compositor == crate::vdisplay::Compositor::Gamescope;
            if game != built_in_game {
                if joined {
                    self.facts
                        .life
                        .end(crate::events::SessionEndReason::HostEnded);
                } else {
                    let _ = catch_up.send(SessionSwitch::seat(game));
                }
            }
            return;
        }
        if !(session_watch_enabled(self.facts.knobs.session_watch)
            && pf_host_config::config().compositor.is_none())
        {
            return;
        }
        tracing::info!("session watcher on — following a mid-stream Gaming↔Desktop switch");
        let (session_tx, session_rx) = std::sync::mpsc::channel::<SessionSwitch>();
        self.ends.session_rx = session_rx;
        let stop = self.facts.life.stop_flag().clone();
        self._watcher = std::thread::Builder::new()
            .name("punktfunk1-watcher".into())
            .spawn(move || session_watcher_loop(session_tx, stop))
            .ok();
    }

    /// The tick loop, then [`Self::finish`]. Every phase is a method; the order is the contract.
    pub(super) fn run(mut self) -> Result<()> {
        // Concurrent sessions interleave in one log; this stamps every line below with
        // the id `/status` reports. Sync body, so the guard never straddles an await.
        let _session_span = tracing::info_span!("session", id = self.live_session.id).entered();
        self.facts.life.enter(crate::lifecycle::Phase::Streaming);
        self.spawn_session_watcher();
        self.publish_arrival_poke();
        let result = self.tick_loop();
        self.finish(result.is_ok());
        result
    }

    fn tick_loop(&mut self) -> Result<()> {
        while !self.facts.life.stopping() && std::time::Instant::now() < self.facts.deadline {
            self.on_session_switch();
            self.on_mode_switch();
            self.on_screens();
            self.on_format_change();
            self.on_topology_reassert()?;
            self.on_fec_moved();
            self.on_bitrate_request();
            self.on_recovery_requests();
            let Some(tick) = self.capture_tick()? else {
                break;
            };
            self.tick_cursor();
            #[cfg(target_os = "linux")]
            self.park_seat_pointer();
            self.log_diag();
            if !self.follow_source_mode()? {
                continue;
            }
            let (t_cap, repeat) = (tick.t_cap, tick.repeat);
            match self.encode_and_send(tick)? {
                Flow::Next => {}
                Flow::Continue => continue,
                Flow::Break => break,
            }
            self.adapt_to_cadence();
            self.sleep_to_next(t_cap, repeat);
        }
        Ok(())
    }

    /// Both ends reach this: the clean tail and a `?` out of the loop. A clean loop drains
    /// the encoder (an errored one is not polled); then the send thread is joined, the
    /// registry gets this session's totals, and the reason is latched. Only a panic skips
    /// it, which the summary reads as `host_error`; the send thread's own drop joins it then.
    pub(super) fn finish(mut self, clean: bool) {
        self.facts.life.enter(crate::lifecycle::Phase::Ending);
        if clean {
            self.drain();
        }
        if let Some(set) = self.screens.as_mut() {
            set.shutdown();
        }
        self.send.join();
        // Source against wire: `source_seq` is what DWM composed into the driver's pool,
        // `dropped` what the pool refused. A source under the refresh rate is the desktop.
        let src = self.live.capturer.supervised().map(|s| s.health());
        tracing::info!(
            sent = self.sent,
            source_seq = src.as_ref().map_or(0, |h| h.source_seq),
            published = src.as_ref().map_or(0, |h| h.published_total),
            dropped = src.as_ref().map_or(0, |h| h.dropped_total),
            "punktfunk/1 virtual stream complete"
        );
        crate::session_status::record_tally(
            self.live_session.id,
            crate::session_status::SessionTally {
                frames_sent: self.sent,
                frames_dropped: src.as_ref().map(|h| h.dropped_total),
                path_mtu: self.facts.conn.current_mtu(),
            },
        );
        self.facts.life.end(if clean {
            crate::events::SessionEndReason::HostEnded
        } else {
            crate::events::SessionEndReason::HostError
        });
    }
}

/// Store the encoder's opened rate and tell the client. Silent when nothing changed.
pub(super) fn adopt_built_bitrate(
    current: &mut u32,
    built: u32,
    live: &Arc<AtomicU32>,
    retarget: &tokio::sync::mpsc::UnboundedSender<(u32, AckReason)>,
) {
    if built == *current {
        return;
    }
    tracing::info!(
        from_kbps = *current,
        to_kbps = built,
        "adopted the rebuilt pipeline's bitrate (re-resolved for what it actually encodes)"
    );
    *current = built;
    live.store(built, Ordering::Relaxed);
    // The host re-resolved what it encodes; nothing refused the client a rate.
    let _ = retarget.send((built, AckReason::Granted));
}

/// Bring-up's first phase: what the session encodes, as the handshake settled it.
fn session_plan(ctx: &SessionContext) -> crate::session_plan::SessionPlan {
    let c = &ctx.common;
    let plan = resolve_plan(
        &ctx.encoder,
        c.bit_depth,
        c.hdr,
        c.chroma,
        c.codec,
        ctx.format_facts.studio_asked,
        ctx.cursor_forward,
        ctx.slices,
        ctx.compositor,
        ctx.gamescope_route.as_ref(),
        ctx.pyrowave_bpp,
        c.session.shard_payload(),
        ctx.reframe_to,
        ctx.join_live,
    );
    tracing::info!(?plan, "resolved session plan");
    plan
}

/// The cursor plan the session's display opens with: the forwarder when the client draws the
/// pointer, else whether the video composites gamescope's XFixes pointer or the metadata cursor.
type CursorPlan = (
    Option<super::super::cursor_fwd::CursorForwarder>,
    bool,
    bool,
);

/// The display request: the cursor plan, and what every display of this session opens with.
/// Bring-up's launch rides the params; a rebuild swaps in its own cursor and route, and drops
/// the launch. `hdr` is the HDR verdict, not the depth: a 10-bit SDR session leaves the output
/// SDR.
fn display_request(
    ctx: &SessionContext,
    plan: &crate::session_plan::SessionPlan,
) -> (CursorPlan, crate::vdisplay::SessionParams) {
    let cursor_fwd = ctx
        .cursor_forward
        .then(super::super::cursor_fwd::CursorForwarder::new);
    if ctx.cursor_forward {
        tracing::info!("cursor channel negotiated — forwarding shape/state, encoder blend off");
    }
    let (gamescope_composite, metadata_composite) = composite_plan(
        plan,
        cursor_fwd.is_some(),
        ctx.compositor == pf_vdisplay::Compositor::Gamescope,
    );
    if gamescope_composite {
        tracing::info!("gamescope cursor: compositing the XFixes-sourced pointer into the video");
    }
    if metadata_composite {
        tracing::info!(
            "no cursor channel — compositing the metadata cursor into the video (this \
             compositor never embeds a pointer on a virtual stream)"
        );
    }
    let params = crate::vdisplay::SessionParams {
        client_fp: ctx.conn.peer_fingerprint(),
        client_hdr: ctx.client_hdr,
        hdr: ctx.common.hdr,
        hw_cursor: ctx.cursor_forward || metadata_composite,
        join_live: ctx.join_live,
        quit: ctx.common.life.quit_flag().clone(),
        launch: ctx.launch.clone(),
        route: ctx.gamescope_route.clone(),
        #[cfg(target_os = "linux")]
        isolation: ctx.isolation.clone(),
        #[cfg(not(target_os = "linux"))]
        isolation: None,
    };
    (
        (cursor_fwd, gamescope_composite, metadata_composite),
        params,
    )
}

/// Whether AUs go out streamed, then sliced: the client accepts streamed AUs and the session's
/// knobs leave them on (`PUNKTFUNK_STREAMED_AU=0` reverts to whole-AU sends). Encoder chunking
/// is per AU.
fn wire_request(ctx: &SessionContext) -> (bool, bool) {
    let streamed = ctx.streamed_au && ctx.knobs.streamed_au;
    if streamed {
        tracing::info!(
            "client accepts streamed AUs (VIDEO_CAP_STREAMED_AU) — used if this session's \
             encoder supports chunked output"
        );
    }
    (
        streamed,
        streamed && ctx.slices > 1 && ctx.knobs.slice_stream,
    )
}

/// The mode and total wire budget (kbps) bring-up builds at, and whether it adopted a mode the
/// control task accepted meanwhile. Adopting builds once: two RecordVirtual monitors ~400 ms
/// apart segfault mutter inside `meta_monitor_manager_rebuild`. A prepared pipeline stays as
/// built. PyroWave's pin follows an adopted mode.
fn settle_mode(
    ctx: &SessionContext,
    plan: &crate::session_plan::SessionPlan,
    prepared: bool,
) -> (punktfunk_core::Mode, u32, bool) {
    let c = &ctx.common;
    let (mut mode, mut bitrate_kbps) = (c.mode, c.bitrate_kbps);
    let queued = if prepared {
        None
    } else {
        c.ends.reconfig.try_iter().last()
    };
    let adopted = queued.filter(|m| *m != mode);
    if let Some(m) = adopted {
        tracing::info!(
            stale = ?mode,
            adopted = ?m,
            "a mode switch was accepted before bring-up finished — building at the new mode \
             instead of building twice"
        );
        mode = m;
        if plan.codec == crate::encode::Codec::PyroWave {
            let running = ctx.bitrate_auto.then_some(bitrate_kbps);
            bitrate_kbps = pyrowave_mode_kbps(running, &mode, plan, ctx.knobs.pyrowave_max_kbps);
        }
    }
    tracing::info!(
        compositor = ctx.compositor.id(),
        ?mode,
        bitrate_kbps,
        bit_depth = c.bit_depth,
        "punktfunk/1 virtual display"
    );
    (mode, bitrate_kbps, adopted.is_some())
}

/// The session's facts at the mode it builds, on handles of their own. `new` stamps the
/// deadline once the stream is up.
fn session_facts(
    ctx: &SessionContext,
    plan: &crate::session_plan::SessionPlan,
    mode: punktfunk_core::Mode,
    streamed_wire: bool,
) -> SessionFacts {
    let c = &ctx.common;
    SessionFacts {
        deadline: std::time::Instant::now(),
        encoder: ctx.encoder.clone(),
        knobs: ctx.knobs,
        life: c.life.clone(),
        counters: c.counters.clone(),
        conn: ctx.conn.clone(),
        negotiated: mode,
        bitrate_auto: ctx.bitrate_auto,
        audio_reserved_kbps: c.audio_reserved_kbps,
        shard_payload: c.shard_payload,
        pyrowave: plan.codec == crate::encode::Codec::PyroWave,
        streamed_wire,
        perf: pf_host_config::config().perf,
        client_hdr: ctx.client_hdr,
        bringup: c.bringup.clone(),
        stats: c.stats.clone(),
        format_facts: ctx.format_facts.clone(),
    }
}

/// The loop's handles on what other threads read, and the FEC it applies. The send thread's
/// counters and the registry's handles start here, before either holds them.
fn shared_handles(ctx: &SessionContext) -> (Shared, FecState) {
    let s = &ctx.common.shared;
    let shared = Shared {
        acked: s.acked.clone(),
        lossy_window: s.lossy_window.clone(),
        arrival_poke: s.arrival_poke.clone(),
        driver_dropped: Default::default(),
        screen_wire: s.screens.clone(),
        send_spread_us: Default::default(),
        wire_rekeys: Default::default(),
        live_mode: Default::default(),
        force_idr: Default::default(),
        capture_health: Default::default(),
        frame_map: ctx.frame_map.clone(),
        #[cfg(target_os = "linux")]
        gamescope_xwayland: ctx.gamescope_xwayland.clone(),
        resize_ms: ctx.resize_ms.clone(),
        live_bitrate: s.live_bitrate.clone(),
        encoder_ceiling: s.encoder_ceiling.clone(),
        cadence_degraded: s.cadence_degraded.clone(),
        cadence_behind_score: s.cadence_behind_score.clone(),
        client_packets_received: s.client_packets_received.clone(),
        cursor_client_draws: s.cursor_client_draws.clone(),
    };
    let fec = FecState {
        requested: s.fec_requested.clone(),
        target: s.fec_target.clone(),
        last: s.fec_target.load(Ordering::Relaxed),
        pending: None,
        hold_logged: 0,
    };
    (shared, fec)
}

/// The session's display and its request: the Welcome-time prep's display with the pipeline it
/// built, or one opened now with no pipeline yet.
fn open_display(
    prepared: Option<PreparedDisplay>,
    compositor: crate::vdisplay::Compositor,
    vd_params: crate::vdisplay::SessionParams,
    (cursor_fwd, gamescope_composite, metadata_composite): CursorPlan,
) -> Result<(DisplayRequest, Option<Pipeline>)> {
    let (vd, pipe) = match prepared {
        Some(p) => (p.vd, Some(p.pipeline)),
        None => {
            // Open first: Windows `open` inits the manager; `vdm()` before that panics.
            let mut vd = crate::vdisplay::open(compositor)?;
            vd_params.apply(&mut *vd);
            (vd, None)
        }
    };
    let display = DisplayRequest {
        vd,
        compositor,
        cursor_fwd,
        cursor_client_drew: true,
        gamescope_composite,
        metadata_composite,
        // Settled against the portal once the first pipeline is up.
        #[cfg(target_os = "linux")]
        no_overlay_means_off_output: false,
        vd_params,
        #[cfg(target_os = "linux")]
        parked_display: None,
        #[cfg(target_os = "linux")]
        park_attempts: 0,
        #[cfg(target_os = "linux")]
        next_park_at: std::time::Instant::now(),
        #[cfg(target_os = "linux")]
        moves_at_arm: 0,
    };
    Ok((display, pipe))
}

/// Build the first pipeline on the session's display at `bitrate_kbps`, over bring-up's eight
/// attempts. PyroWave's parity rides each frame's head at the mode built, and the first screen
/// takes the whole video until further screens move its share. Windows holds the slot's IDD
/// setup guard across the build: it preempts only a prior session on this client's slot.
fn build_first(
    display: &mut DisplayRequest,
    facts: &SessionFacts,
    plan: crate::session_plan::SessionPlan,
    bit_depth: u8,
    bitrate_kbps: u32,
    fec: &FecState,
) -> Result<Pipeline> {
    let mode = facts.negotiated;
    let _idd_setup_guard = crate::windows::idd::setup_guard(
        plan.capture,
        display.vd_params.client_fp,
        (mode.width, mode.height),
        facts.life.stop_flag(),
    )?;
    let req = BuildRequest {
        mode,
        bitrate_kbps,
        bitrate_auto: facts.bitrate_auto,
        bit_depth,
        enc_of: EncDerive {
            audio_kbps: facts.audio_reserved_kbps,
            shard_payload: facts.shard_payload,
            fec_percent: fec.target.load(Ordering::Relaxed),
            head_only_hz: facts.pyrowave.then_some(mode.refresh_hz),
            share_permille: crate::native::bitrate::SHARE_ALL,
        },
        plan,
        pyrowave_max_kbps: facts.knobs.pyrowave_max_kbps,
        encoder: facts.encoder.clone(),
        client_hdr: facts.client_hdr,
        wire_seq_base: 0,
        screen: 0,
    };
    build_pipeline_with_retry(
        &mut display.vd,
        &req,
        facts.life.quit_flag(),
        facts.life.stop_flag(),
        None,
        1..=8,
        Some(facts.bringup.as_ref()),
    )
}

/// The first pipeline as the loop's live one at `mode`. The input thread's framing, the
/// clipboard's gamescope and the portal's cursor verdict follow it, the rate it opened at is
/// adopted, and `/status` gets the mode it delivers.
fn first_live(
    pipe: Pipeline,
    display: &mut DisplayRequest,
    shared: &Shared,
    mode: punktfunk_core::Mode,
    mut bitrate_kbps: u32,
    retarget: &tokio::sync::mpsc::UnboundedSender<(u32, AckReason)>,
) -> Live {
    let Pipeline {
        capturer,
        enc,
        frame,
        interval,
        node_id,
        display_gen,
        bitrate_kbps: built,
        reframe,
        #[cfg(target_os = "linux")]
        lease,
        #[cfg(target_os = "linux")]
        panel_tick_ns,
    } = pipe;
    *shared.frame_map.lock().unwrap_or_else(|e| e.into_inner()) = reframe;
    #[cfg(target_os = "linux")]
    {
        publish_gamescope_xwayland(&shared.gamescope_xwayland, lease.as_ref());
        display.no_overlay_means_off_output =
            settle_portal_cursor(&*display.vd, &mut display.metadata_composite);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = display;
    adopt_built_bitrate(&mut bitrate_kbps, built, &shared.live_bitrate, retarget);
    // Stats slot only — an ordinary connect is not owed a corrective Reconfigured.
    let delivered = delivered_mode(frame.width, frame.height, interval);
    shared.live_mode.store(
        pack_mode(delivered.width, delivered.height, delivered.refresh_hz),
        Ordering::Relaxed,
    );
    Live {
        enc_src: (frame.format, frame.width, frame.height),
        capturer,
        enc,
        frame,
        interval,
        cur_node_id: node_id,
        cur_display_gen: display_gen,
        epoch: 0,
        #[cfg(target_os = "linux")]
        lease,
        #[cfg(target_os = "linux")]
        restamp: panel_tick_ns.map(super::encode::SourceStamps::new),
        cur_mode: mode,
        bitrate_kbps,
        retargeted: false,
        asks: Default::default(),
        applied_share: crate::native::bitrate::SHARE_ALL,
    }
}

/// An adopted mode is not the one the control task's first config names: tell it what epoch 0
/// delivers. It sends only what the client does not hold, and corrects a client acked with
/// another mode.
fn tell_adopted(
    tell: &tokio::sync::mpsc::UnboundedSender<crate::native::wiring::Delivered>,
    live: &Live,
    plan: &crate::session_plan::SessionPlan,
    bit_depth: u8,
) {
    let actual = delivered_mode(live.frame.width, live.frame.height, live.interval);
    let _ = tell.send(crate::native::wiring::Delivered {
        mode: actual,
        format: format_of(plan, &*live.capturer, bit_depth),
        epoch: 0,
        corrects: actual != live.cur_mode,
    });
}

/// Start the send thread on the data plane the ramp hands back, and the resend hook the control
/// task's NACKs ride beside the frames. A ramp step in flight finishes first, which is ≤ 50 ms.
/// Depth 3: encode blocks if send falls behind, rather than drop a frame (infinite GOP freeze).
/// Returns the thread and a sender for the further screens.
#[allow(clippy::too_many_arguments)]
fn start_send(
    ramp: ramp::RampServer,
    facts: &SessionFacts,
    shared: &Shared,
    wiring: &SessionShared,
    probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    shard_rx: std::sync::mpsc::Receiver<usize>,
    timing_conn: Option<crate::native::link::SessionLink>,
    probe_seq: bool,
    wire_sock: Option<std::net::UdpSocket>,
    client_label: &str,
    codec: crate::encode::Codec,
    slice_wire: bool,
) -> Result<(
    super::send::SendThread,
    Arc<std::sync::mpsc::SyncSender<SendMsg>>,
)> {
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<SendMsg>(3);
    let frame_tx = Arc::new(frame_tx);
    let nack_tx = Arc::downgrade(&frame_tx);
    *wiring.resend.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(Box::new(move |frame, shards| {
            nack_tx
                .upgrade()
                .is_some_and(|tx| tx.try_send(SendMsg::Resend { frame, shards }).is_ok())
        }));
    let stats = SendStats {
        rec: facts.stats.clone(),
        mode: shared.live_mode.clone(),
        codec: codec.label(),
        client: client_label.to_owned(),
        plane: facts.conn.plane(),
        bitrate_kbps: shared.live_bitrate.clone(),
        link_kbps: wiring.link_kbps.clone(),
        ports: wiring.ports,
        link_paced: facts.pyrowave && !facts.bitrate_auto,
        refuted: wiring.refuted.clone(),
        shape: wiring.shape.clone(),
        bringup: facts.bringup.clone(),
        wire_sock,
        driver_dropped: shared.driver_dropped.clone(),
        counters: facts.counters.clone(),
    };
    let (session, probe_rx) = ramp.finish()?;
    let stop = facts.life.stop_flag().clone();
    let perf = facts.perf;
    let send_spread_us = shared.send_spread_us.clone();
    let wire_rekeys = shared.wire_rekeys.clone();
    let fec_target = wiring.fec_target.clone();
    let thread = std::thread::Builder::new()
        .name("punktfunk-send".into())
        .spawn(move || {
            send_loop(
                session,
                frame_rx,
                probe_rx,
                probe_result_tx,
                stop,
                perf,
                send_spread_us,
                wire_rekeys,
                slice_wire,
                fec_target,
                shard_rx,
                stats,
                timing_conn,
                probe_seq,
            )
        })
        .context("spawn send thread")?;
    Ok((
        super::send::SendThread::new(frame_tx.clone(), thread),
        frame_tx,
    ))
}

/// Publish the session to `/status` with the handles its routes act on.
#[allow(clippy::too_many_arguments)]
fn register_session(
    facts: &SessionFacts,
    shared: &Shared,
    plan: &crate::session_plan::SessionPlan,
    bit_depth: u8,
    join: bool,
    client: String,
    client_name: Option<String>,
    controls: crate::session_status::SessionControls,
    game: Option<Arc<crate::gamelease::LeaseShared>>,
) -> crate::session_status::LiveSessionGuard {
    crate::session_status::register(crate::session_status::Registration {
        mode: shared.live_mode.clone(),
        bitrate_kbps: shared.live_bitrate.clone(),
        codec: plan.codec,
        life: facts.life.clone(),
        force_idr: shared.force_idr.clone(),
        client,
        client_name,
        plane: facts.conn.plane(),
        hdr: plan.hdr,
        ttff_ms: facts.bringup.total_slot(),
        last_resize_ms: shared.resize_ms.clone(),
        game,
        capture_health: shared.capture_health.clone(),
        join,
        controls,
        bit_depth,
        chroma: plan.chroma,
        counters: facts.counters.clone(),
        peer: Some(facts.conn.remote_address().ip()),
    })
}

/// Launch the session's title on the display it just opened (generation `cur_display_gen`),
/// tell the client the verdict, and take the game lease that follows it, with the
/// dedicated-Steam exit watcher. The display's params carry the launch command, route and quit
/// flag. Returns the lease's shared view for the registry and the guard that ends it with the
/// session.
#[allow(clippy::too_many_arguments)]
fn spawn_launch_and_lease(
    display: &DisplayRequest,
    cur_display_gen: Option<u64>,
    facts: &SessionFacts,
    launch_target: Option<&crate::library::LaunchTarget>,
    launch_owner: &crate::session_launch::LaunchOwner,
    launch_claim: Option<crate::launchreg::Claim>,
    launch_stamp: Option<f64>,
    launch_outcome: &crate::gamelease::OutcomeTx,
) -> (
    Option<Arc<crate::gamelease::LeaseShared>>,
    Option<crate::gamelease::SessionGuard>,
) {
    let (vd, vd_params, compositor) = (&*display.vd, &display.vd_params, display.compositor);
    let (conn, life) = (&facts.conn, &facts.life);
    #[cfg(not(target_os = "linux"))]
    let _ = (vd, compositor, cur_display_gen);
    #[cfg(target_os = "linux")]
    let (launch, gamescope_route) = (vd_params.launch.as_deref(), vd_params.route.as_ref());
    let quit = &vd_params.quit;
    // This session's compositor, by pool generation: a concurrent seat's gamescope is equally
    // discoverable in `/proc`, so an unscoped launch or watch lands on somebody else's screen.
    #[cfg(target_os = "linux")]
    let seat: Option<String> = cur_display_gen.and_then(crate::vdisplay::registry::seat_for);
    // The head the lease's window stage places the game on. Read here, where capture
    // has already published it and a later session cannot have re-pointed the
    // injector's one-per-process slot yet.
    #[cfg(target_os = "linux")]
    let streamed_head = crate::inject::stream_output()
        .map(|output| crate::session_status::StreamedHead { compositor, output });
    // This acquire spawned gamescope itself, so the launch is its primary child.
    #[cfg(target_os = "linux")]
    let nested_spawn = crate::vdisplay::launch_is_nested(compositor, gamescope_route)
        && vd.nested_launch_started();
    let spawned = crate::session_launch::spawn(
        launch_target,
        launch_owner,
        launch_claim.as_ref(),
        #[cfg(target_os = "linux")]
        crate::session_launch::SpawnAt {
            compositor,
            nested_spawn,
            seat: seat.as_deref(),
        },
    );
    let spawned_now = spawned.now;
    if let Some(t) = launch_target {
        let _ = launch_outcome.send(launch_verdict(
            &t.game.title,
            launch_claim.as_ref(),
            spawned_now,
        ));
    }
    // A dedicated Steam session ends on gamescope's own root atoms, never on process shape:
    // Steam wraps its pre-launch work (shader precompile, the install-script evaluator) in the
    // same `SteamLaunch AppId=` reaper the game gets, so a scan adopts a tree that was never the
    // game and reads its exit as the game exiting, seconds before the game starts.
    #[cfg(target_os = "linux")]
    let steam_exit_appid: Option<u32> = launch
        .filter(|_| crate::vdisplay::launch_is_nested(compositor, gamescope_route))
        .and_then(crate::vdisplay::steam_appid_from_launch);
    #[cfg(not(target_os = "linux"))]
    let steam_exit_appid: Option<u32> = None;

    let end_on_game_exit = {
        let conn = conn.clone();
        let life = life.clone();
        crate::session_launch::end_on_game_exit(move || {
            tracing::info!("the launched game exited — ending the session cleanly (APP_EXITED)");
            life.end(crate::events::SessionEndReason::GameExited);
            conn.close(punktfunk_core::quic::APP_EXITED_CLOSE_CODE, b"game exited");
        })
    };

    let game_lease = launch_target.map(|target| {
        let on_exit: crate::gamelease::OnExit = if steam_exit_appid.is_some() {
            Box::new(|| {
                tracing::info!(
                    "game lease: the launched game exited (status only — this dedicated Steam \
                     session ends on gamescope's atoms)"
                );
            })
        } else {
            Box::new(end_on_game_exit.clone())
        };
        let extras = crate::session_launch::LeaseExtras {
            #[cfg(target_os = "linux")]
            nested: crate::vdisplay::launch_is_nested(compositor, gamescope_route),
            // Two seats can play the same title and Steam's reaper looks the same in both,
            // so recognition narrows to this session's gamescope where it may
            // ([`crate::gamelease::scan_scope`]).
            #[cfg(target_os = "linux")]
            scope_pid: crate::gamelease::scan_scope(
                nested_spawn,
                launch.is_some_and(crate::vdisplay::launch_is_steam),
                cur_display_gen.and_then(crate::vdisplay::registry::compositor_pid_for),
            ),
            #[cfg(not(target_os = "linux"))]
            nested: false,
            #[cfg(not(target_os = "linux"))]
            scope_pid: None,
            // The watcher says so when this launch dies on the spot.
            outcome: Some(launch_outcome.clone()),
            window: window_source(
                #[cfg(target_os = "linux")]
                compositor,
                #[cfg(target_os = "linux")]
                streamed_head.clone(),
                #[cfg(target_os = "linux")]
                seat.clone(),
                target.detect.steam_appid,
                target.on_window,
            ),
        };
        crate::session_launch::lease(
            target,
            launch_owner,
            launch_stamp,
            launch_claim.as_ref(),
            spawned,
            extras,
            on_exit,
        )
    });
    let game_shared = game_lease.as_ref().map(|l| l.shared());
    // The atom watcher owns the end and `game.exited` for a dedicated Steam session;
    // `gamelease` keeps running, so the console still shows what is playing, but it no longer
    // closes the connection.
    #[cfg(target_os = "linux")]
    if let Some(appid) = steam_exit_appid {
        let stop = life.stop_flag().clone();
        let end = end_on_game_exit.clone();
        let seat = seat.clone();
        let game = game_shared.clone();
        let spawned = std::thread::Builder::new()
            .name("pf1-steamexit".into())
            .spawn(move || {
                if crate::vdisplay::watch_steam_game_exit(appid, seat.as_deref(), &stop) {
                    if let Some(g) = game.as_deref() {
                        crate::gamelease::report_exit(g);
                    }
                    end();
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "dedicated Steam exit watcher not started");
        }
    }
    let game_life = game_lease.map(|lease| {
        crate::gamelease::SessionGuard::new(
            lease,
            quit.clone(),
            conn.peer_fingerprint().map(hex::encode),
            launch_claim,
        )
    });
    (game_shared, game_life)
}

/// What this session's launch came to, in the client's vocabulary.
///
/// One verdict from the facts the launch site has: whether it spawned, and what the
/// registry adopted against. `Spawned` says nothing — the player asked for a game and is
/// about to get one; the rest need words.
fn launch_verdict(
    title: &str,
    claim: Option<&crate::launchreg::Claim>,
    spawned: bool,
) -> punktfunk_core::quic::LaunchOutcome {
    use crate::launchreg::Liveness;
    use punktfunk_core::quic::{LaunchOutcome, LaunchOutcomeKind as Kind};
    if spawned {
        return LaunchOutcome::new(Kind::Spawned, "");
    }
    match claim.and_then(|c| c.adopted()) {
        Some(Liveness::Running) => LaunchOutcome::new(
            Kind::Adopted,
            &format!(
                "{title} was already running from an earlier session — this picked that copy up \
                 instead of starting a second one."
            ),
        ),
        // Adopted on the in-flight window: the host reused a launch it cannot see.
        Some(_) => LaunchOutcome::new(
            Kind::AdoptedUnknown,
            &format!(
                "{title} was started a moment ago, so this picked that launch up rather than \
                 starting a second copy. Start it again if nothing comes up."
            ),
        ),
        None => LaunchOutcome::new(
            Kind::Refused,
            &format!("Couldn't start {title} — this host had nothing to run for it."),
        ),
    }
}

/// Point the clipboard at the gamescope behind `lease`. A lease with no seat is the box's own
/// gamescope, which the unscoped lookup finds; a non-gamescope session never reads this.
#[cfg(target_os = "linux")]
pub(super) fn publish_gamescope_xwayland(
    xwayland: &pf_clipboard::GamescopeXwayland,
    lease: Option<&crate::capture::OutputLease>,
) {
    let seat = lease.and_then(|l| l.seat()).map(str::to_owned);
    xwayland.set(move || {
        pf_vdisplay::gamescope_xwayland_cursor_targets(seat.as_deref())
            .into_iter()
            .map(|(display, _)| display)
            .collect()
    });
}

/// The session registry's client label, on every plane: the 12-hex prefix of the device's
/// certificate fingerprint, else its address, so one device reads the same on both planes and
/// unpairing it finds its sessions. Empty with neither.
pub(crate) fn session_client_label(
    fingerprint_hex: Option<&str>,
    peer: Option<std::net::IpAddr>,
) -> String {
    fingerprint_hex
        .map(|fp| fp[..12.min(fp.len())].to_string())
        .or_else(|| peer.map(|ip| ip.to_string()))
        .unwrap_or_default()
}

/// Announce a host-local rebuild gap so the client does not score a straddling window as congestion.
pub(super) fn announce_pipeline_gap(gap: &tokio::sync::mpsc::UnboundedSender<u32>, gap_ms: u32) {
    if gap_ms == 0 {
        return;
    }
    let _ = gap.send(gap_ms);
}

/// [`announce_pipeline_gap`] for a stall that began at `t0`: every rebuild the loop sits in,
/// failed ones too, since a failed build stalls the stream as long.
pub(super) fn announce_gap_since(
    gap: &tokio::sync::mpsc::UnboundedSender<u32>,
    t0: std::time::Instant,
) {
    announce_pipeline_gap(
        gap,
        t0.elapsed().as_millis().min(u128::from(u32::MAX)) as u32,
    );
}

/// Where this session's lease looks for the game's window: the compositor's own window list,
/// gamescope's focused-app atom for a Steam title, or the Windows desktop. `None` when gamescope
/// runs something other than a Steam title.
fn window_source(
    #[cfg(target_os = "linux")] compositor: crate::vdisplay::Compositor,
    #[cfg(target_os = "linux")] head: Option<crate::session_status::StreamedHead>,
    #[cfg(target_os = "linux")] seat: Option<String>,
    steam_appid: Option<u32>,
    on_window: crate::library::OnWindow,
) -> Option<crate::gamelease::WindowSource> {
    #[cfg(target_os = "linux")]
    {
        use crate::gamelease::WindowSource;
        use crate::vdisplay::Compositor;
        match compositor {
            Compositor::Hyprland
            | Compositor::Wlroots
            | Compositor::Niri
            | Compositor::Umbriel
            | Compositor::Kwin
            | Compositor::Mutter => Some(WindowSource::Toplevels {
                compositor,
                stage: head.map(|head| crate::gamelease::WindowStage { head, on_window }),
            }),
            Compositor::Gamescope => {
                steam_appid.map(|appid| WindowSource::Gamescope { seat, appid })
            }
            Compositor::Windows => None,
        }
    }
    #[cfg(windows)]
    {
        let _ = (steam_appid, on_window);
        Some(crate::gamelease::WindowSource::Desktop)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (steam_appid, on_window);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PyroWave rebuild prices its head parity at the refresh it builds at, not the one the
    /// session ran before the switch.
    #[test]
    fn a_rebuild_prices_parity_at_the_mode_it_builds() {
        let mut f = super::super::swap::tests::fake();
        f.st.facts.pyrowave = true;
        let to = punktfunk_core::Mode {
            refresh_hz: f.st.live.cur_mode.refresh_hz * 2,
            ..f.st.live.cur_mode
        };
        let req = f.st.build_request(to, f.st.live.bitrate_kbps);
        assert_eq!(req.enc_of.head_only_hz, Some(to.refresh_hz));
    }

    /// A swapped-in encoder opened at the first screen's share in force, whatever the swap's
    /// reason, so the screens' next turn owes it no retarget.
    #[test]
    fn a_swap_takes_the_share_its_encoder_opened_at() {
        use super::super::swap::{Next, SwapReason};
        for why in [
            SwapReason::SessionSwitch,
            SwapReason::ModeSwitch,
            SwapReason::TopologyReassert,
            SwapReason::CaptureLost,
            SwapReason::SourceMode,
            SwapReason::Bitrate,
            SwapReason::Format,
        ] {
            let mut f = super::super::swap::tests::fake();
            let wire = &f.st.shared.screen_wire;
            wire.primary_share.store(500, Ordering::Relaxed);
            let next = Next::Encoder {
                enc: Box::new(super::super::swap::tests::NullEnc),
                reframe: Default::default(),
                resized: None,
            };
            f.st.swap_pipeline(next, why);
            assert_eq!(f.st.live.applied_share, 500, "{why:?}");
        }
    }

    /// An epoch says what the source composes: an HDR session over an SDR capture is 8-bit
    /// BT.709, an SDR session is what it negotiated.
    #[test]
    fn an_epoch_tells_the_format_the_source_composes() {
        use crate::encode::{ChromaFormat, Codec};
        use punktfunk_core::quic::ColorInfo;
        let sdr = crate::capture::SyntheticCapturer::new(16, 16, 60);
        let encoder = crate::encode::resolve();
        let plan = |hdr| {
            crate::session_plan::SessionPlan::resolve(
                &encoder,
                10,
                hdr,
                ChromaFormat::Yuv420,
                Codec::H265,
                false,
                false,
                1,
            )
        };
        let f = format_of(&plan(true), &sdr, 10);
        assert_eq!((f.bit_depth, f.color), (8, ColorInfo::SDR_BT709));
        let f = format_of(&plan(false), &sdr, 10);
        assert_eq!((f.bit_depth, f.color), (10, ColorInfo::SDR_BT709));
    }

    /// Native passes the connection's fingerprint and address; GameStream the paired
    /// certificate's fingerprint, if read, and the launch owner's address, if bound.
    #[test]
    fn both_planes_label_a_client_alike() {
        let fp = "0123456789abcdef".repeat(4);
        let ip = std::net::IpAddr::from([192, 168, 1, 21]);
        assert_eq!(session_client_label(Some(&fp), Some(ip)), "0123456789ab");
        assert_eq!(session_client_label(None, Some(ip)), "192.168.1.21");
        assert_eq!(session_client_label(Some(&fp), None), "0123456789ab");
        assert_eq!(session_client_label(Some("abc"), None), "abc");
        assert_eq!(session_client_label(None, None), "");
    }

    #[test]
    fn adopting_a_rebuilt_rate_tells_the_client() {
        let live = Arc::new(AtomicU32::new(20_000));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u32, AckReason)>();
        let mut current = 20_000;
        adopt_built_bitrate(&mut current, 20_000, &live, &tx);
        assert_eq!(rx.try_recv().ok(), None);
        adopt_built_bitrate(&mut current, 60_000, &live, &tx);
        assert_eq!(current, 60_000);
        assert_eq!(live.load(Ordering::Relaxed), 60_000);
        // Nobody refused the client anything: the host re-resolved its own rate.
        assert_eq!(rx.try_recv().ok(), Some((60_000, AckReason::Granted)));
    }

    /// The registry's liveness vocabulary and the wire's are one set, mapped here
    /// and nowhere else. A spawn says nothing; a refusal and a blind adoption
    /// both owe the player a sentence.
    #[test]
    fn the_launch_verdict_follows_what_the_registry_adopted() {
        use punktfunk_core::quic::LaunchOutcomeKind as Kind;

        let spawned = launch_verdict("Quail", None, true);
        assert_eq!(spawned.kind, Kind::Spawned);
        assert!(spawned.message.is_empty());
        assert!(!spawned.kind.needs_telling());

        let refused = launch_verdict("Quail", None, false);
        assert_eq!(refused.kind, Kind::Refused);
        assert!(refused.message.starts_with("Couldn't start Quail"));
        assert!(refused.kind.needs_telling());

        let (fp, app) = (Some("fp-verdict"), Some("custom:verdict"));
        let first = crate::launchreg::claim(fp, app, false, Some(1.0));
        first.launched();
        // Nothing adopted, inside the in-flight window: the host cannot see it.
        let blind = crate::launchreg::claim(fp, app, false, Some(2.0));
        assert!(!blind.must_spawn());
        let out = launch_verdict("Quail", Some(&blind), false);
        assert_eq!(out.kind, Kind::AdoptedUnknown);
        assert!(out.message.contains("Start it again"));
        assert!(out.kind.needs_telling());
        blind.abandon();
        drop(first);
    }
}
