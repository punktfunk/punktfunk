//! The virtual-display stream loop's state. [`StreamState::new`] is bring-up (display,
//! pipeline, launch, game lease, send thread); [`StreamState::run`] is the tick loop, one
//! method per concern:
//!
//! - `rebuild.rs`: session switch, mode switch, topology re-assert, capture loss, source-mode
//!   follow — everything that swaps the pipeline under the loop.
//! - `recovery.rs`: FEC/bitrate re-derivation and the keyframe/RFI coalescing.
//! - `encode.rs`: capture tick, submit, poll, stall watch, depth adaptation, pacing sleep.
//! - `cursor.rs`: cursor forwarding, host composite, the seat-pointer park.
//! - `resize.rs`: the Windows in-place resize.
//!
//! Fields are the loop's former locals. A per-tick value (`repeat`, `t_cap`, `owed`) travels in
//! [`Tick`], not here.

use super::cursor::composite_plan;
#[cfg(target_os = "linux")]
use super::cursor::settle_portal_cursor;
use super::pipeline::{build_pipeline_with_retry, Pipeline};
use super::*;

/// Non-blocking poll returning None forever while submits succeed. 2 s also sizes the backlog bound.
pub(super) const ENCODE_STALL_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
pub(super) const MAX_ENCODER_RESETS: u32 = 5;
pub(super) const MAX_CAPTURE_REBUILDS: u32 = 5;
/// (capture_ns, submit_ns, send deadline) per frame handed to the encoder and not yet polled.
pub(super) type Inflight = std::collections::VecDeque<(u64, u64, std::time::Instant)>;

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

pub(super) struct StreamState {
    // ---- loop bookkeeping ----
    pub(super) deadline: std::time::Instant,
    pub(super) next: std::time::Instant,
    pub(super) sent: u64,
    /// Survives in-loop rebuilds so a mid-stream rebuild keeps the acquired lock.
    pub(super) phase_ctl: PhaseController,
    /// Same: a rebuild must not reopen the overshoot.
    pub(super) pace: CaptureCredit,
    /// Predicted as `au_seq + inflight.len()`. Encoder-internal counters desync on the first ABR rebuild.
    pub(super) au_seq: u32,
    /// A chunked AU's FIRST went out and its LAST has not: `au_seq` still names that frame.
    pub(super) wire_frame_open: bool,
    pub(super) capture_rebuilds: u32,
    /// Topology re-assert generation this loop last saw. Only Windows IDD-push moves it.
    pub(super) seen_reassert_gen: u64,
    pub(super) encoder_resets: u32,
    pub(super) last_au_at: std::time::Instant,
    pub(super) last_hdr_meta: Option<pf_frame::HdrMeta>,
    pub(super) inflight: Inflight,
    /// NEW source frames / REPEATS / cursor REGENS since `diag_at`. Logged every 2 s under `PUNKTFUNK_PERF`.
    pub(super) diag_new: u64,
    pub(super) diag_repeat: u64,
    pub(super) diag_regen: u64,
    pub(super) diag_at: std::time::Instant,
    #[cfg(target_os = "linux")]
    pub(super) parked_display: Option<(u32, Option<u64>)>,
    #[cfg(target_os = "linux")]
    pub(super) park_attempts: u32,
    #[cfg(target_os = "linux")]
    pub(super) next_park_at: std::time::Instant,
    /// Each host-composite outcome is logged once.
    pub(super) composite_log: super::cursor::CompositeLog,
    pub(super) last_forced_idr: Option<std::time::Instant>,
    /// Never re-anchors the IDR cooldown: sustained loss + RFI would swallow IDR pleas forever.
    pub(super) last_rfi: Option<std::time::Instant>,
    pub(super) kf_gate: super::recovery::KeyframeGate,
    pub(super) recovery_cadence: pf_frame::metronome::Metronome,
    pub(super) ir_wave_pos: u32,
    pub(super) st_cap: Vec<u32>,
    pub(super) st_submit: Vec<u32>,
    pub(super) st_wait: Vec<u32>,
    pub(super) st_queue: Vec<u32>,
    /// The Windows driver's pool drops, handed to the send thread's recorder sample.
    pub(super) driver_dropped: Arc<AtomicU64>,
    pub(super) cur_depth: usize,
    pub(super) behind_score: u32,
    pub(super) last_fec: u8,
    pub(super) depth_frames: u64,
    /// EMA of real-frame arrivals. Negotiated refresh is the wrong deadline when the game is slower.
    pub(super) src_period_ns: Option<u64>,
    /// Longest host submit+poll chain observed this tick, independent of perf sampling.
    pub(super) encode_chain_ns: u64,
    pub(super) last_real_cap: Option<std::time::Instant>,
    pub(super) was_degraded: bool,
    pub(super) last_cadence_log: Option<std::time::Instant>,
    pub(super) cadence_flips_suppressed: u32,
    pub(super) pipeline_asked: bool,
    pub(super) pipelined_active: bool,
    pub(super) deescalating: bool,
    pub(super) ahead_run: u32,
    pub(super) deescalate_not_before: Option<std::time::Instant>,
    pub(super) deescalate_backoff: std::time::Duration,

    // ---- teardown order, which is field order: the status registration ends first, then
    // the send thread, then game policy, then capture, encoder, display ----
    _watcher: Option<std::thread::JoinHandle<()>>,
    pub(super) live_session: crate::session_status::LiveSessionGuard,
    // ---- the send thread ----
    pub(super) frame_tx: std::sync::mpsc::SyncSender<SendMsg>,
    pub(super) send_thread: std::thread::JoinHandle<()>,
    pub(super) send_spread_us: Arc<AtomicU32>,
    pub(super) wire_rekeys: Arc<AtomicU32>,
    pub(super) live_mode: Arc<AtomicU64>,
    pub(super) force_idr: Arc<AtomicBool>,
    pub(super) capture_health: Arc<std::sync::Mutex<Option<pf_capture::CaptureHealth>>>,
    pub(super) health_published_at: std::time::Instant,
    _game_life: Option<crate::gamelease::SessionGuard>,

    // ---- the live pipeline ----
    pub(super) capturer: Box<dyn crate::capture::Capturer>,
    pub(super) enc: Box<dyn crate::encode::Encoder>,
    pub(super) frame: crate::capture::CapturedFrame,
    pub(super) interval: std::time::Duration,
    pub(super) cur_node_id: u32,
    pub(super) cur_display_gen: Option<u64>,
    /// The live output's metadata for a capture-only rebuild (`on_capture_lost`).
    #[cfg(target_os = "linux")]
    pub(super) lease: Option<super::pipeline::OutputLease>,
    /// Source can change format/size with no client Reconfigure; in-place encoder reset cannot follow.
    pub(super) enc_src: (pf_frame::PixelFormat, u32, u32),
    /// The mode a rebuild reopens at: the client's latest ask, or the source's delivered size.
    pub(super) cur_mode: punktfunk_core::Mode,
    /// Total wire budget (kbps). Only encoder opens convert via [`EncDerive`].
    pub(super) bitrate_kbps: u32,
    pub(super) vd: Box<dyn crate::vdisplay::VirtualDisplay>,
    pub(super) compositor: crate::vdisplay::Compositor,
    pub(super) cursor_fwd: Option<super::super::cursor_fwd::CursorForwarder>,
    /// Starts true so the first composite request triggers the capturer hook.
    pub(super) cursor_client_drew: bool,
    pub(super) gamescope_composite: bool,
    pub(super) metadata_composite: bool,
    #[cfg(target_os = "linux")]
    pub(super) no_overlay_means_off_output: bool,

    // ---- fixed for the session ----
    pub(super) plan: crate::session_plan::SessionPlan,
    pub(super) stop: Arc<AtomicBool>,
    pub(super) quit: Arc<AtomicBool>,
    /// Why this session ended, for its summary. First write wins, so the path that knows
    /// (game exit, operator stop, the peer's own close) beats the loop's clean tail.
    pub(super) end_reason: Arc<std::sync::atomic::AtomicU8>,
    /// Session totals; this loop notes every encoder rate it adopts.
    pub(super) counters: Arc<crate::session_status::SessionCounters>,
    pub(super) conn: super::super::link::SessionLink,
    /// The client's ask. Encoders fit to it; the display may deliver another size.
    pub(super) negotiated: punktfunk_core::Mode,
    pub(super) bitrate_auto: bool,
    pub(super) bit_depth: u8,
    pub(super) audio_reserved_kbps: u32,
    pub(super) shard_payload: u16,
    /// PyroWave: the wire budget is the encoder rate.
    pub(super) budget_identity: bool,
    pub(super) streamed_wire: bool,
    pub(super) perf: bool,
    pub(super) launch: Option<String>,
    pub(super) client_hdr: Option<pf_frame::HdrMeta>,
    /// Admitted by `mode_conflict: join`. A rebuild's new display asks to share again.
    pub(super) join_live: bool,
    /// The live encoder's framing: forwarded cursor positions map through it, and so does
    /// the input thread's absolute input. Written on every encoder open.
    pub(super) frame_map: super::super::input::FrameMap,
    pub(super) bringup: Arc<crate::bringup::Trace>,
    pub(super) resize_ms: Arc<AtomicU32>,
    pub(super) stats: Arc<StatsRecorder>,
    pub(super) phase: Arc<PhaseCtl>,
    /// Applied FEC: what the packetizer and [`Self::enc_now`] run at.
    pub(super) fec_target: Arc<AtomicU8>,
    /// Control task's proposal; applied only after the encoder takes its rate.
    pub(super) fec_requested: Arc<AtomicU8>,
    pub(super) live_bitrate: Arc<AtomicU32>,
    pub(super) encoder_ceiling: Arc<std::sync::Mutex<super::EncoderCeiling>>,
    /// A rate was handed to this encoder after it opened, so a later read-back
    /// is about that retarget. What a pipeline opened at is the build's own
    /// business (`Pipeline::bitrate_kbps`), and is not re-litigated here.
    pub(super) retargeted: bool,
    /// A FEC proposal an asynchronous encoder was asked to make room for: the
    /// parity and the encoder rate it implies, kbps. Parity waits for the
    /// encoder to settle.
    pub(super) fec_pending: Option<(u8, u32)>,
    /// The proposal a hold was last logged for, so a refusal the control task
    /// re-proposes every window logs once.
    pub(super) fec_hold_logged: u8,
    pub(super) cadence_degraded: Arc<AtomicBool>,
    pub(super) cadence_behind_score: Arc<AtomicU32>,
    pub(super) client_packets_received: Arc<AtomicU32>,
    pub(super) cursor_shape_tx:
        tokio::sync::watch::Sender<Option<punktfunk_core::quic::CursorShape>>,
    pub(super) cursor_client_draws: Arc<AtomicBool>,
    #[cfg(target_os = "linux")]
    pub(super) gamescope_route: Option<crate::vdisplay::GamescopeRoute>,
    #[cfg(target_os = "linux")]
    pub(super) input_tx: std::sync::mpsc::SyncSender<super::super::input::ClientInput>,
    #[cfg(target_os = "linux")]
    pub(super) isolation: Option<crate::vdisplay::SessionIsolation>,
    #[cfg(target_os = "linux")]
    pub(super) input_route: super::super::input::InputRoute,
    #[cfg(target_os = "linux")]
    pub(super) inj_shared_tx: std::sync::mpsc::Sender<punktfunk_core::input::InputEvent>,
    #[cfg(target_os = "linux")]
    pub(super) inj_session_tx: Option<std::sync::mpsc::Sender<punktfunk_core::input::InputEvent>>,

    // ---- control-plane inputs ----
    pub(super) reconfig: std::sync::mpsc::Receiver<punktfunk_core::Mode>,
    pub(super) keyframe: std::sync::mpsc::Receiver<()>,
    pub(super) rfi: std::sync::mpsc::Receiver<(u32, u32)>,
    pub(super) bitrate_rx: std::sync::mpsc::Receiver<u32>,
    pub(super) session_rx: std::sync::mpsc::Receiver<SessionSwitch>,
    pub(super) reconfig_result_tx: tokio::sync::mpsc::UnboundedSender<Reconfigured>,
    pub(super) retarget_tx: tokio::sync::mpsc::UnboundedSender<(u32, AckReason)>,
    pub(super) gap_tx: tokio::sync::mpsc::UnboundedSender<u32>,
}

impl StreamState {
    /// The encoder-rate derivation for a FEC percentage.
    pub(super) fn enc_derive(&self, fec: u8) -> super::super::EncDerive {
        super::super::EncDerive {
            audio_kbps: self.audio_reserved_kbps,
            shard_payload: self.shard_payload,
            fec_percent: fec,
            identity: self.budget_identity,
        }
    }

    /// [`Self::enc_derive`] at the FEC target in force now.
    pub(super) fn enc_now(&self) -> super::super::EncDerive {
        self.enc_derive(self.fec_target.load(Ordering::Relaxed))
    }

    /// Swap the built pipeline in and forget every owed AU and the last forwarded cursor shape:
    /// a new capturer numbers its shapes from 1 again. The caller retires the old lease,
    /// re-arms the IDR clock, and re-reads `enc_src` as its path requires.
    pub(super) fn adopt_pipeline(&mut self, p: Pipeline) {
        // A ceiling was learned from the encoder this one replaces. It survives
        // a rebuild that opens on the same source; a different geometry or
        // format is a different encoder, whose limits are unknown again.
        if (p.frame.format, p.frame.width, p.frame.height)
            != (self.frame.format, self.frame.width, self.frame.height)
        {
            self.encoder_ceiling
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
        self.retargeted = false;
        self.fec_pending = None;
        self.adopt_reframe(p.reframe);
        self.capturer = p.capturer;
        if let Some(fwd) = self.cursor_fwd.as_mut() {
            *fwd = super::super::cursor_fwd::CursorForwarder::new();
        }
        self.enc = p.enc;
        self.frame = p.frame;
        self.interval = p.interval;
        self.cur_node_id = p.node_id;
        self.cur_display_gen = p.display_gen;
        #[cfg(target_os = "linux")]
        {
            self.lease = p.lease;
        }
        self.inflight.clear();
        self.last_au_at = std::time::Instant::now();
        self.encoder_resets = 0;
    }

    pub(super) fn adopt_reframe(&self, reframe: punktfunk_core::video_fit::Reframe) {
        *self.frame_map.lock().unwrap_or_else(|e| e.into_inner()) = reframe;
    }

    /// Lease drop looks like a disconnect to keep-alive; retire or linger accumulates.
    pub(super) fn retire_replaced_gen(&self, old: Option<u64>) {
        if let Some(g) = old.filter(|g| self.cur_display_gen != Some(*g)) {
            crate::vdisplay::registry::retire(g);
        }
    }

    /// A rebuild re-resolved what it encodes. Noted for the summary's bitrate span: the
    /// rate the session runs at moved, whoever decided it.
    pub(super) fn adopt_built_bitrate(&mut self, built: u32) {
        if built != self.bitrate_kbps {
            self.counters.note_bitrate(built);
        }
        adopt_built_bitrate(
            &mut self.bitrate_kbps,
            built,
            &self.live_bitrate,
            &self.retarget_tx,
        );
    }

    pub(super) fn delivered_mode(&self) -> punktfunk_core::Mode {
        delivered_mode(self.frame.width, self.frame.height, self.interval)
    }

    /// Publish the delivered mode to `/status` and correct the client when it differs from `asked`.
    pub(super) fn publish_delivered_mode(&self, asked: punktfunk_core::Mode) {
        let actual = self.delivered_mode();
        self.live_mode.store(
            pack_mode(actual.width, actual.height, actual.refresh_hz),
            Ordering::Relaxed,
        );
        if actual != asked {
            let _ = self.reconfig_result_tx.send(Reconfigured {
                accepted: true,
                mode: actual,
            });
        }
    }

    /// Bring the session up: display, pipeline, library launch, game lease, send thread.
    pub(super) fn new(ctx: SessionContext, prepared: Option<PreparedDisplay>) -> Result<Self> {
        let mut plan = crate::session_plan::SessionPlan::resolve(
            ctx.bit_depth,
            ctx.hdr,
            ctx.chroma,
            ctx.codec,
            crate::session_plan::cursor_blend_for(
                ctx.cursor_forward,
                ctx.compositor,
                ctx.codec,
                ctx.bit_depth,
                ctx.hdr,
                ctx.gamescope_route.as_ref(),
            ),
            ctx.cursor_forward,
            ctx.multi_slice,
        );
        // After resolve: a self-painting gamescope node would otherwise get a second XFixes pointer.
        plan.gamescope_cursor = crate::session_plan::gamescope_cursor_for(
            ctx.compositor == pf_vdisplay::Compositor::Gamescope,
            ctx.gamescope_route.as_ref(),
        );
        if ctx.codec == crate::encode::Codec::PyroWave {
            plan.wire_chunk = Some(ctx.session.shard_payload());
        }
        plan.reframe_to = ctx.reframe_to;
        tracing::info!(?plan, "resolved session plan");
        // Automatic PyroWave: the client's ramp closes with one lower pin, so
        // the window lingers past pipeline-ready for it to cross.
        let fit_pin = ctx.bitrate_auto && ctx.codec == crate::encode::Codec::PyroWave;
        let SessionContext {
            session: punched_session,
            mode,
            seconds,
            stop,
            quit,
            end_reason,
            counters,
            reconfig,
            keyframe,
            rfi,
            bitrate_rx,
            shard_rx,
            compositor,
            gamescope_route,
            mut bitrate_kbps,
            audio_reserved_kbps,
            shard_payload,
            live_bitrate,
            encoder_ceiling,
            cadence_degraded,
            cadence_behind_score,
            client_packets_received,
            bitrate_auto,
            bit_depth,
            hdr,
            chroma: _,
            codec: _,
            probe_rx,
            probe_result_tx,
            ramp_open,
            reconfig_result_tx,
            retarget_tx,
            gap_tx,
            fec_target,
            fec_requested,
            link_kbps,
            conn,
            timing_conn,
            phase,
            cursor_forward,
            cursor_shape_tx,
            cursor_client_draws,
            probe_seq,
            streamed_au,
            multi_slice,
            stats,
            client_label,
            client_name,
            launch,
            launch_target,
            launch_claim,
            fresh_stamp,
            launch_outcome,
            client_hdr,
            join_live,
            controls,
            reframe_to: _,
            frame_map,
            bringup,
            resize_ms,
            wire_sock,
            #[cfg(target_os = "linux")]
            input_tx,
            #[cfg(target_os = "linux")]
            isolation,
            #[cfg(target_os = "linux")]
            input_route,
            #[cfg(target_os = "linux")]
            inj_shared_tx,
            #[cfg(target_os = "linux")]
            inj_session_tx,
        } = ctx;
        // The data plane is punched and idle until the send thread starts.
        // Answer the client's bring-up ramp on it meanwhile: it measures the
        // link with no video to damage, and hands both back below.
        let ramp = ramp::RampServer::start(
            punched_session,
            probe_rx,
            probe_result_tx.clone(),
            probe_seq,
            stop.clone(),
            ramp_open,
            fit_pin,
        );
        // Adopt against the original stamp, or procscan refuses the running game.
        let launch_stamp = launch_claim.as_ref().map_or(fresh_stamp, |c| c.stamp());
        // `PUNKTFUNK_STREAMED_AU=0` reverts to whole-AU sends. Encoder chunking is per-AU.
        // `bitrate_kbps` is the total wire budget; only encoder opens convert via EncDerive.
        let budget_identity = plan.codec == crate::encode::Codec::PyroWave;
        let enc_derive = move |fec: u8| super::super::EncDerive {
            audio_kbps: audio_reserved_kbps,
            shard_payload,
            fec_percent: fec,
            identity: budget_identity,
        };
        let streamed_wire =
            streamed_au && std::env::var("PUNKTFUNK_STREAMED_AU").as_deref() != Ok("0");
        let slice_wire = streamed_wire
            && multi_slice
            && std::env::var("PUNKTFUNK_SLICE_STREAM").as_deref() != Ok("0");
        let cursor_fwd = cursor_forward.then(super::super::cursor_fwd::CursorForwarder::new);
        if cursor_forward {
            tracing::info!("cursor channel negotiated — forwarding shape/state, encoder blend off");
        }
        #[allow(unused_mut)] // Linux settles it against the portal below.
        let (gamescope_composite, mut metadata_composite) = composite_plan(
            &plan,
            cursor_fwd.is_some(),
            compositor == pf_vdisplay::Compositor::Gamescope,
        );
        if gamescope_composite {
            tracing::info!(
                "gamescope cursor: compositing the XFixes-sourced pointer into the video"
            );
        }
        if metadata_composite {
            tracing::info!(
                "no cursor channel — compositing the metadata cursor into the video (this \
                 compositor never embeds a pointer on a virtual stream)"
            );
        }
        if streamed_wire {
            tracing::info!(
                "client accepts streamed AUs (VIDEO_CAP_STREAMED_AU) — used if this session's \
                 encoder supports chunked output"
            );
        }
        // Adopt a mode accepted before bring-up and build once. Two RecordVirtual monitors ~400 ms
        // apart segfault mutter inside `meta_monitor_manager_rebuild`. Prepared pipelines stay as-is.
        let mut mode = mode;
        let mut adopted_at_bringup = false;
        if prepared.is_none() {
            let mut queued = None;
            while let Ok(m) = reconfig.try_recv() {
                queued = Some(m);
            }
            if let Some(m) = queued.filter(|m| *m != mode) {
                adopted_at_bringup = true;
                tracing::info!(
                    stale = ?mode,
                    adopted = ?m,
                    "a mode switch was accepted before bring-up finished — building at the new mode \
                     instead of building twice"
                );
                mode = m;
                if bitrate_auto && plan.codec == crate::encode::Codec::PyroWave {
                    bitrate_kbps =
                        resolve_bitrate_kbps_for(plan.codec, 0, &mode, plan.chroma, plan.bit_depth);
                }
            }
        }
        tracing::info!(
            compositor = compositor.id(),
            ?mode,
            bitrate_kbps,
            bit_depth,
            "punktfunk/1 virtual display"
        );
        let (vd, pipe) = match prepared {
            Some(p) => (p.vd, p.pipeline),
            None => {
                // Open first: Windows `open` inits the manager; `vdm()` before that panics.
                let mut vd = crate::vdisplay::open(compositor)?;
                vd.set_client_identity(conn.peer_fingerprint());
                vd.set_join_live(join_live);
                vd.set_client_hdr(client_hdr);
                // HDR verdict, not the depth — a 10-bit SDR session leaves the output SDR.
                vd.set_hdr(hdr);
                vd.set_hw_cursor(cursor_forward || metadata_composite);
                vd.set_quit_flag(quit.clone());
                vd.set_launch_command(launch.clone());
                vd.set_gamescope_route(gamescope_route.clone());
                #[cfg(target_os = "linux")]
                vd.set_session_isolation(isolation.clone());
                // Slot-scoped: preempt only a prior session on THIS client's slot. Held before create.
                let _idd_setup_guard = crate::windows::idd::setup_guard(
                    plan.capture,
                    conn.peer_fingerprint(),
                    (mode.width, mode.height),
                    &stop,
                )?;
                let pipe = build_pipeline_with_retry(
                    &mut vd,
                    mode,
                    bitrate_kbps,
                    bitrate_auto,
                    bit_depth,
                    enc_derive(fec_target.load(Ordering::Relaxed)),
                    plan,
                    &quit,
                    &stop,
                    None,
                    8,
                    Some(bringup.as_ref()),
                    client_hdr,
                    0,
                )?;
                (vd, pipe)
            }
        };
        let Pipeline {
            capturer,
            enc,
            frame,
            interval,
            node_id: cur_node_id,
            display_gen: cur_display_gen,
            bitrate_kbps: built_bitrate,
            reframe,
            #[cfg(target_os = "linux")]
            lease,
        } = pipe;
        *frame_map.lock().unwrap_or_else(|e| e.into_inner()) = reframe;
        let enc_src = (frame.format, frame.width, frame.height);
        #[cfg(target_os = "linux")]
        let no_overlay_means_off_output = settle_portal_cursor(&*vd, &mut metadata_composite);
        adopt_built_bitrate(
            &mut bitrate_kbps,
            built_bitrate,
            &live_bitrate,
            &retarget_tx,
        );
        if adopted_at_bringup {
            let actual = delivered_mode(frame.width, frame.height, interval);
            if actual != mode {
                let _ = reconfig_result_tx.send(Reconfigured {
                    accepted: true,
                    mode: actual,
                });
            }
        }

        // Once per launch, not per session. Mid-stream rebuilds must not re-spawn.
        let adopt_launch = launch_claim.as_ref().is_some_and(|c| !c.must_spawn());
        #[allow(unused_mut)]
        let mut spawned_now = false;
        // A forwarder's pid (`WinRecipe::owns_game` false) is not a lifetime signal.
        #[allow(unused_mut)]
        let mut spawned_pid: Option<u32> = None;
        if !adopt_launch {
            if let Some(t) = launch_target.as_ref() {
                crate::gamelease::end_others_for_new_launch(
                    conn.peer_fingerprint().map(hex::encode).as_deref(),
                    t.game.id.as_deref(),
                );
            }
        }
        #[cfg(target_os = "windows")]
        if let Some(id) = launch.as_deref() {
            if adopt_launch {
                tracing::info!(
                    launch_id = id,
                    "this client's copy of this title is already running from an earlier session — not \
                     starting a second one"
                );
            } else {
                match crate::library::launch_title(id) {
                    Ok(launched) => {
                        spawned_pid = launched.tracked_pid();
                        spawned_now = true;
                    }
                    Err(e) => {
                        tracing::warn!(launch_id = id, error = %e, "requested library title not launched")
                    }
                }
            }
        }
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
        // Workspace this launch owns on the streamed head; handed to the lease, which
        // releases it when the game is done.
        #[cfg(target_os = "linux")]
        let mut launch_workspace: Option<crate::vdisplay::WorkspaceClaim> = None;
        // This acquire spawned gamescope itself, so the launch is its primary child. A keep-alive
        // reuse spawned nothing and launches into the live session instead.
        #[cfg(target_os = "linux")]
        let nested_spawn = crate::vdisplay::launch_is_nested(compositor, gamescope_route.as_ref())
            && vd.nested_launch_started();
        #[cfg(target_os = "linux")]
        let spawned_launch = match launch.as_deref() {
            Some(cmd) if adopt_launch => {
                tracing::info!(
                    command = %cmd,
                    "this client's copy of this title is already running from an earlier session — not \
                     starting a second one"
                );
                // The claim belongs to the launch, not to us: go back to the game's
                // workspace rather than opening an empty one beside it.
                launch_workspace = launch_claim
                    .as_ref()
                    .and_then(|c| c.workspace())
                    .and_then(|ws| crate::library::adopt_launch_workspace(compositor, ws));
                None
            }
            // Nested only when this acquire actually spawned gamescope — then `cmd` is already its
            // primary child. A keep-alive reuse spawned nothing, so it falls through and launches
            // into the live session below; without that, a second launch showed an idle session.
            Some(cmd) if nested_spawn => {
                tracing::info!(command = %cmd, "launch nested into the per-session gamescope");
                spawned_now = true;
                None
            }
            Some(cmd) => {
                let own = launch_target.as_ref().is_some_and(|t| t.own_workspace);
                // A reuse spawned nothing, so the launch goes to the live session — under this
                // seat's Steam home when it has one.
                let seat_steam = isolation.as_ref().and_then(|i| i.steam_home.as_deref());
                match crate::library::launch_session_command(
                    compositor,
                    cmd,
                    seat.as_deref(),
                    own,
                    seat_steam,
                ) {
                    Ok(mut spawned) => {
                        spawned_now = true;
                        launch_workspace = spawned.workspace.take();
                        Some(spawned)
                    }
                    Err(e) => {
                        tracing::warn!(command = %cmd, error = %e, "requested title not launched into the session");
                        None
                    }
                }
            }
            None => None,
        };
        // A Steam launch that ran under this seat's own home: remember what it streamed at, so
        // the host can have that Steam up before this device's next connect. `vd`'s own values,
        // not the request, because they are the registry's reuse keys.
        #[cfg(target_os = "linux")]
        if spawned_now
            && launch
                .as_deref()
                .is_some_and(crate::vdisplay::launch_is_steam)
        {
            if let Some(fp) = isolation
                .as_ref()
                .filter(|i| i.steam_home.is_some())
                .and_then(|_| conn.peer_fingerprint())
            {
                crate::native::prewarm::record(&hex::encode(fp), mode, vd.hdr(), vd.hw_cursor());
            }
        }
        // This seat's Steam has no account, so the stream shows its sign-in screen and not the
        // game. Read before the verdict: the player is told what to do, and no client holds this
        // title's cover over the screen they have to act on.
        #[cfg(target_os = "linux")]
        let seat_sign_in = spawned_now
            && launch
                .as_deref()
                .is_some_and(crate::vdisplay::launch_is_steam)
            && isolation
                .as_ref()
                .is_some_and(crate::vdisplay::seat_needs_sign_in);
        #[cfg(not(target_os = "linux"))]
        let seat_sign_in = false;
        if let Some(t) = launch_target.as_ref() {
            let _ = launch_outcome.send(launch_verdict(
                &t.game.title,
                launch_claim.as_ref(),
                spawned_now,
                seat_sign_in,
            ));
        }
        if let Some(c) = launch_claim.as_ref() {
            if spawned_now {
                c.launched();
                if let Some(id) = c.credits() {
                    crate::library::record_launch(id);
                }
            } else if c.must_spawn() {
                c.abandon();
            }
            // On the record, not on the session: the next reconnect focuses it.
            #[cfg(target_os = "linux")]
            if let Some(ws) = launch_workspace.as_ref() {
                c.placed(ws.id());
            }
        }

        // A dedicated Steam session ends on gamescope's own root atoms, never on process shape:
        // Steam wraps its pre-launch work (shader precompile, the install-script evaluator) in the
        // same `SteamLaunch AppId=` reaper the game gets, so a scan adopts a tree that was never the
        // game and reads its exit as the game exiting, seconds before the game starts.
        #[cfg(target_os = "linux")]
        let steam_exit_appid: Option<u32> = launch
            .as_deref()
            .filter(|_| crate::vdisplay::launch_is_nested(compositor, gamescope_route.as_ref()))
            .and_then(crate::vdisplay::steam_appid_from_launch);
        #[cfg(not(target_os = "linux"))]
        let steam_exit_appid: Option<u32> = None;

        let end_on_game_exit = {
            let conn = conn.clone();
            let stop = stop.clone();
            let quit = quit.clone();
            let end_reason = end_reason.clone();
            move || {
                if !crate::session_settings::get().session_on_game_exit {
                    tracing::info!(
                        "the launched game exited, but ending the session on game exit is off — \
                         leaving the stream up"
                    );
                    return;
                }
                tracing::info!(
                    "the launched game exited — ending the session cleanly (APP_EXITED)"
                );
                crate::events::SessionEndReason::GameExited.latch(&end_reason);
                conn.close(punktfunk_core::quic::APP_EXITED_CLOSE_CODE, b"game exited");
                quit.store(true, Ordering::SeqCst);
                stop.store(true, Ordering::SeqCst);
            }
        };

        // The atom watcher owns the end for a dedicated Steam session; `gamelease` keeps running, so
        // the console still shows what is playing, but it no longer closes the connection.
        #[cfg(target_os = "linux")]
        if let Some(appid) = steam_exit_appid {
            let stop = stop.clone();
            let end = end_on_game_exit.clone();
            let seat = seat.clone();
            let spawned = std::thread::Builder::new()
                .name("pf1-steamexit".into())
                .spawn(move || {
                    if crate::vdisplay::watch_steam_game_exit(appid, seat.as_deref(), &stop) {
                        end();
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "dedicated Steam exit watcher not started");
            }
        }

        let game_lease = launch_target.as_ref().map(|target| {
            #[cfg(target_os = "linux")]
            let nested = crate::vdisplay::launch_is_nested(compositor, gamescope_route.as_ref());
            #[cfg(not(target_os = "linux"))]
            let nested = false;
            #[cfg(target_os = "linux")]
            let child = spawned_launch.map(|s| (s.child, s.group_leader));
            #[cfg(not(target_os = "linux"))]
            let child = None;

            let on_exit: crate::gamelease::OnExit = if steam_exit_appid.is_some() {
                Box::new(|| {
                    tracing::info!(
                        "game lease: the launched game exited (status only — this dedicated Steam \
                         session ends on gamescope's atoms)"
                    );
                })
            } else {
                Box::new(end_on_game_exit)
            };
            crate::gamelease::open(
                crate::gamelease::LeaseRequest {
                    game: target.game.clone(),
                    client: client_label.clone(),
                    fingerprint: controls.fingerprint.clone(),
                    preset: controls.preset.clone(),
                    plane: crate::events::Plane::Native,
                    spec: target.detect.clone(),
                    nested,
                    // Two seats can play the same title and Steam's reaper looks the same in both,
                    // so recognition narrows to this session's gamescope where it may
                    // ([`crate::gamelease::scan_scope`]).
                    #[cfg(target_os = "linux")]
                    scope_pid: crate::gamelease::scan_scope(
                        nested_spawn,
                        launch
                            .as_deref()
                            .is_some_and(crate::vdisplay::launch_is_steam),
                        cur_display_gen.and_then(crate::vdisplay::registry::compositor_pid_for),
                    ),
                    #[cfg(not(target_os = "linux"))]
                    scope_pid: None,
                    launcher: target.launcher,
                    child,
                    spawned: spawned_pid,
                    launch_stamp,
                    procs: launch_claim.as_ref().and_then(|c| c.procs()),
                    // The watcher says so when this launch dies on the spot.
                    outcome: Some(launch_outcome.clone()),
                    #[cfg(target_os = "linux")]
                    workspace: launch_workspace,
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
                },
                on_exit,
            )
        });
        let game_shared = game_lease.as_ref().map(|l| l.shared());
        // The watcher keeps its own grace: the game the player starts after signing in is
        // followed as any other.
        if seat_sign_in {
            if let Some(g) = game_shared.as_ref() {
                g.launch_hold_ends();
            }
            tracing::info!(
                "this seat's Steam has no account yet — the stream shows its sign-in screen"
            );
        }
        let game_life = game_lease.map(|lease| {
            crate::gamelease::SessionGuard::new(
                lease,
                quit.clone(),
                conn.peer_fingerprint().map(hex::encode),
                launch_claim,
            )
        });

        let perf = pf_host_config::config().perf;
        let burst_cap: Option<usize> = std::env::var("PUNKTFUNK_PACE_BURST_KB")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .map(|kb| kb * 1024);

        // Depth 3: encode blocks if send falls behind, rather than drop a frame (infinite GOP freeze).
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<SendMsg>(3);
        // Stats slot only — an ordinary connect is not owed a corrective Reconfigured.
        let delivered = delivered_mode(frame.width, frame.height, interval);
        let live_mode = Arc::new(AtomicU64::new(pack_mode(
            delivered.width,
            delivered.height,
            delivered.refresh_hz,
        )));
        let force_idr = Arc::new(AtomicBool::new(false));
        let send_spread_us = Arc::new(AtomicU32::new(0));
        let send_spread_send = Arc::clone(&send_spread_us);
        let wire_rekeys = Arc::new(AtomicU32::new(0));
        let wire_rekeys_send = Arc::clone(&wire_rekeys);
        let driver_dropped = Arc::new(AtomicU64::new(0));
        let send_stats = SendStats {
            rec: stats.clone(),
            mode: live_mode.clone(),
            codec: plan.codec.label(),
            client: client_label.clone(),
            bitrate_kbps: live_bitrate.clone(),
            link_kbps,
            link_paced: budget_identity,
            bringup: bringup.clone(),
            wire_sock,
            driver_dropped: driver_dropped.clone(),
            counters: counters.clone(),
        };
        // Pipeline, launch and lease are up: take the data plane back. A step
        // in flight finishes first, which is ≤ 50 ms.
        let (session, probe_rx) = ramp.finish();
        let send_thread = std::thread::Builder::new()
            .name("punktfunk-send".into())
            .spawn({
                let stop = stop.clone();
                let phase_send = phase.clone();
                let fec_target_send = fec_target.clone();
                move || {
                    send_loop(
                        session,
                        frame_rx,
                        probe_rx,
                        probe_result_tx,
                        stop,
                        perf,
                        send_spread_send,
                        wire_rekeys_send,
                        slice_wire,
                        burst_cap,
                        fec_target_send,
                        shard_rx,
                        send_stats,
                        timing_conn,
                        phase_send,
                        probe_seq,
                    )
                }
            })
            .context("spawn send thread")?;

        let capture_health: Arc<std::sync::Mutex<Option<pf_capture::CaptureHealth>>> =
            Arc::new(std::sync::Mutex::new(None));
        let live_session = crate::session_status::register(crate::session_status::Registration {
            mode: live_mode.clone(),
            bitrate_kbps: live_bitrate.clone(),
            codec: plan.codec,
            stop: stop.clone(),
            quit: quit.clone(),
            force_idr: force_idr.clone(),
            client: client_label,
            client_name,
            plane: crate::events::Plane::Native,
            hdr: plan.hdr,
            ttff_ms: bringup.total_slot(),
            last_resize_ms: resize_ms.clone(),
            game: game_shared,
            capture_health: capture_health.clone(),
            join: join_live,
            controls,
            bit_depth,
            chroma: plan.chroma,
            end_reason: end_reason.clone(),
            counters: counters.clone(),
            peer: Some(conn.remote_address().ip()),
        });

        // Replaced by `spawn_session_watcher` inside the session span; disconnected until then.
        let (_, session_rx) = std::sync::mpsc::channel::<SessionSwitch>();

        let now = std::time::Instant::now();
        Ok(Self {
            plan,
            stop,
            quit,
            end_reason,
            counters,
            conn,
            negotiated: mode,
            bitrate_auto,
            bit_depth,
            audio_reserved_kbps,
            shard_payload,
            budget_identity,
            streamed_wire,
            perf,
            launch,
            client_hdr,
            join_live,
            frame_map,
            bringup,
            resize_ms,
            stats,
            phase,
            fec_target: fec_target.clone(),
            fec_requested: fec_requested.clone(),
            live_bitrate,
            encoder_ceiling,
            retargeted: false,
            fec_pending: None,
            fec_hold_logged: 0,
            cadence_degraded,
            cadence_behind_score,
            client_packets_received,
            cursor_shape_tx,
            cursor_client_draws,
            #[cfg(target_os = "linux")]
            gamescope_route,
            #[cfg(target_os = "linux")]
            input_tx,
            #[cfg(target_os = "linux")]
            isolation,
            #[cfg(target_os = "linux")]
            input_route,
            #[cfg(target_os = "linux")]
            inj_shared_tx,
            #[cfg(target_os = "linux")]
            inj_session_tx,
            reconfig,
            keyframe,
            rfi,
            bitrate_rx,
            session_rx,
            reconfig_result_tx,
            retarget_tx,
            gap_tx,
            frame_tx,
            send_thread,
            send_spread_us,
            wire_rekeys,
            live_mode,
            force_idr,
            capture_health,
            health_published_at: now,
            vd,
            compositor,
            capturer,
            enc,
            frame,
            interval,
            cur_node_id,
            cur_display_gen,
            #[cfg(target_os = "linux")]
            lease,
            enc_src,
            cur_mode: mode,
            bitrate_kbps,
            cursor_fwd,
            cursor_client_drew: true,
            gamescope_composite,
            metadata_composite,
            #[cfg(target_os = "linux")]
            no_overlay_means_off_output,
            deadline: now + std::time::Duration::from_secs(seconds as u64),
            next: now,
            sent: 0,
            phase_ctl: PhaseController::new(),
            pace: CaptureCredit::new(now),
            au_seq: 0,
            wire_frame_open: false,
            capture_rebuilds: 0,
            seen_reassert_gen: crate::windows::idd::topology_reassert_gen(),
            encoder_resets: 0,
            last_au_at: now,
            last_hdr_meta: None,
            inflight: Inflight::new(),
            diag_new: 0,
            diag_repeat: 0,
            diag_regen: 0,
            diag_at: now,
            #[cfg(target_os = "linux")]
            parked_display: None,
            #[cfg(target_os = "linux")]
            park_attempts: 0,
            #[cfg(target_os = "linux")]
            next_park_at: now,
            composite_log: super::cursor::CompositeLog::default(),
            // Pipeline opened on an IDR — start the clock so the cold-GOP keyframe storm coalesces.
            last_forced_idr: Some(now),
            last_rfi: None,
            kf_gate: super::recovery::KeyframeGate::default(),
            recovery_cadence: pf_frame::metronome::Metronome::new(),
            ir_wave_pos: 0,
            st_cap: Vec::new(),
            st_submit: Vec::new(),
            st_wait: Vec::new(),
            st_queue: Vec::new(),
            driver_dropped,
            cur_depth: 1,
            behind_score: 0,
            last_fec: fec_target.load(Ordering::Relaxed),
            depth_frames: 0,
            src_period_ns: None,
            encode_chain_ns: 0,
            last_real_cap: None,
            was_degraded: false,
            last_cadence_log: None,
            cadence_flips_suppressed: 0,
            pipeline_asked: false,
            pipelined_active: false,
            deescalating: false,
            ahead_run: 0,
            deescalate_not_before: None,
            deescalate_backoff: super::encode::DEESCALATE_BACKOFF_START,
            live_session,
            _watcher: None,
            _game_life: game_life,
        })
    }

    /// Follow a mid-stream Gaming↔Desktop switch unless `PUNKTFUNK_COMPOSITOR` pins the backend.
    fn spawn_session_watcher(&mut self) {
        if !(session_watch_enabled() && pf_host_config::config().compositor.is_none()) {
            return;
        }
        tracing::info!("session watcher on — following a mid-stream Gaming↔Desktop switch");
        let (session_tx, session_rx) = std::sync::mpsc::channel::<SessionSwitch>();
        self.session_rx = session_rx;
        let stop = self.stop.clone();
        self._watcher = std::thread::Builder::new()
            .name("punktfunk1-watcher".into())
            .spawn(move || session_watcher_loop(session_tx, stop))
            .ok();
    }

    /// The tick loop, then the drain. Every phase is a method; the order is the contract.
    ///
    /// Reaching the tail is what makes the end clean: it hands the registry this session's
    /// totals and latches `host_ended`. Any earlier exit leaves both unset, and the summary
    /// reads that as `host_error`.
    pub(super) fn run(mut self) -> Result<()> {
        // Concurrent sessions interleave in one log; this stamps every line below with
        // the id `/status` reports. Sync body, so the guard never straddles an await.
        let _session_span = tracing::info_span!("session", id = self.live_session.id).entered();
        self.spawn_session_watcher();
        while !self.stop.load(Ordering::SeqCst) && std::time::Instant::now() < self.deadline {
            self.on_session_switch();
            self.on_mode_switch();
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
            match self.encode_and_send(tick)? {
                Flow::Next => {}
                Flow::Continue => continue,
                Flow::Break => break,
            }
            self.adapt_depth();
            self.sleep_to_next(tick.t_cap);
        }
        self.drain();
        drop(self.frame_tx);
        let _ = self.send_thread.join();
        // Source against wire: `source_seq` is what DWM composed into the driver's pool,
        // `dropped` what the pool refused. A source under the refresh rate is the desktop.
        let src = self.capturer.health();
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
                path_mtu: self.conn.current_mtu(),
            },
        );
        crate::events::SessionEndReason::HostEnded.latch(&self.end_reason);
        Ok(())
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

/// What this session's launch came to, in the client's vocabulary.
///
/// One verdict from the facts the launch site has: whether it spawned, what the
/// registry adopted against, and whether the seat it spawned into still owes
/// Steam a sign-in. `Spawned` says nothing — the player asked for a game and is
/// about to get one; the rest need words.
fn launch_verdict(
    title: &str,
    claim: Option<&crate::launchreg::Claim>,
    spawned: bool,
    sign_in: bool,
) -> punktfunk_core::quic::LaunchOutcome {
    use crate::launchreg::Liveness;
    use punktfunk_core::quic::{LaunchOutcome, LaunchOutcomeKind as Kind};
    if spawned {
        // The host does not re-send this launch after the sign-in, so the sentence says who does.
        if sign_in {
            return LaunchOutcome::new(
                Kind::SignInNeeded,
                &format!(
                    "Steam on this seat isn't signed in yet. Sign in on the stream, then start \
                     {title} from Big Picture."
                ),
            );
        }
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

/// Announce a host-local rebuild gap so the client does not score a straddling window as congestion.
pub(super) fn announce_pipeline_gap(gap: &tokio::sync::mpsc::UnboundedSender<u32>, gap_ms: u32) {
    if gap_ms == 0 {
        return;
    }
    let _ = gap.send(gap_ms);
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
            Compositor::Hyprland | Compositor::Wlroots | Compositor::Kwin | Compositor::Mutter => {
                Some(WindowSource::Toplevels {
                    compositor,
                    stage: head.map(|head| crate::gamelease::WindowStage { head, on_window }),
                })
            }
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

        let spawned = launch_verdict("Quail", None, true, false);
        assert_eq!(spawned.kind, Kind::Spawned);
        assert!(spawned.message.is_empty());
        assert!(!spawned.kind.needs_telling());

        let refused = launch_verdict("Quail", None, false, false);
        assert_eq!(refused.kind, Kind::Refused);
        assert!(refused.message.starts_with("Couldn't start Quail"));
        assert!(refused.kind.needs_telling());

        let (fp, app) = (Some("fp-verdict"), Some("custom:verdict"));
        let first = crate::launchreg::claim(fp, app, false, Some(1.0));
        first.launched();
        // Nothing adopted, inside the in-flight window: the host cannot see it.
        let blind = crate::launchreg::claim(fp, app, false, Some(2.0));
        assert!(!blind.must_spawn());
        let out = launch_verdict("Quail", Some(&blind), false, false);
        assert_eq!(out.kind, Kind::AdoptedUnknown);
        assert!(out.message.contains("Start it again"));
        assert!(out.kind.needs_telling());
        blind.abandon();
        drop(first);
    }

    /// A seat whose Steam has no account is the one telling verdict that is not a failure: the
    /// launch did reach Steam, and the screen the player has to act on is already streaming.
    /// Every other launch is untouched by it.
    #[test]
    fn a_seat_that_owes_a_sign_in_says_so_instead_of_staying_silent() {
        use punktfunk_core::quic::LaunchOutcomeKind as Kind;

        let out = launch_verdict("Quail", None, true, true);
        assert_eq!(out.kind, Kind::SignInNeeded);
        assert!(out.kind.needs_telling());
        assert!(out.message.contains("isn't signed in"));
        assert!(out.message.contains("Quail"));
        // No seat home, no Steam launch, nothing spawned: byte-for-byte what it said before.
        assert_eq!(
            launch_verdict("Quail", None, true, false).kind,
            Kind::Spawned
        );
        assert_eq!(
            launch_verdict("Quail", None, false, true).kind,
            Kind::Refused
        );
    }
}
