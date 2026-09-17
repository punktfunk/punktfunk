//! Blocking data-plane pump: poll the session, run Adaptive-FEC / ABR /
//! jump-to-live / standing-latency, and hand frames to the embedder.
//!
//! Dedicated user-interactive thread. Newest-frame drop on embedder lag.
//! [`FLAG_PROBE`] filler never enters the decoder. Tests here pin the
//! delivery-report cadence, ABR window activity, probe targets and aftermath, and
//! pipeline-gap window discard.

use super::super::*;
use super::*;
use crate::abr::WindowActivity;

/// Data-plane pump on a blocking thread. `try_send` drops the newest frame
/// when the embedder lags. [`FLAG_PROBE`] filler goes to the probe accumulator,
/// not the decoder.
pub(super) struct DataPump {
    pub(super) session: Session,
    pub(super) frames: Arc<FrameChannel>,
    pub(super) ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    pub(super) shutdown: Arc<std::sync::atomic::AtomicBool>,
    pub(super) probe: Arc<Mutex<ProbeState>>,
    pub(super) hot_tids: Arc<Mutex<Vec<i32>>>,
    pub(super) clock_offset: Arc<std::sync::atomic::AtomicI64>,
    pub(super) clock_gen: Arc<AtomicU32>,
    pub(super) decode_lat: Arc<Mutex<DecodeLatAcc>>,
    /// Host encode-stage window ([`super::super::frame_channel::EncodeLatAcc`]);
    /// fed by the datagram task, not the overlay's lossy `host_timing_tx`.
    pub(super) encode_lat: Arc<Mutex<super::super::frame_channel::EncodeLatAcc>>,
    /// Control-task mode-switch generation. A change resets mode-scoped ABR
    /// state ([`BitrateController::on_mode_switch`]).
    pub(super) mode_gen: Arc<AtomicU32>,
    pub(super) frames_dropped: Arc<std::sync::atomic::AtomicU64>,
    pub(super) fec_recovered: Arc<std::sync::atomic::AtomicU64>,
    /// The pinned rate this client could not hold, kbps; `0` until it sheds
    /// its backlog [`PIN_SHEDS_TO_WARN`] times. Embedders show it once.
    pub(super) unsustainable_pin_kbps: Arc<AtomicU32>,
    /// Host `BitrateChanged` acks, drained in arrival order. A queue so a
    /// corrective short retarget cannot be clobbered by a full resolve ack
    /// in the same window (host-cap learning needs two consecutive shorts).
    pub(super) bitrate_ack: Arc<Mutex<std::collections::VecDeque<u32>>>,
    /// Decode-recovery keyframe asks, counted at the control-task send choke.
    /// Drained per report window as the ABR recovery signal.
    pub(super) recovery_kf: Arc<AtomicU32>,
    /// Host pipeline-rebuild gap in ms ([`crate::quic::PipelineGap`]); `0` =
    /// none. Drained each iteration — see [`take_pipeline_gap`].
    pub(super) pipeline_gap: Arc<AtomicU32>,
    /// Embedder-requested rate. `0` = Automatic (the only case ABR arms).
    pub(super) bitrate_kbps: u32,
    /// Rate the host actually configured (Welcome echo; old host echoes 0).
    pub(super) resolved_bitrate_kbps: u32,
    pub(super) negotiated_codec: u8,
    /// Negotiated bit depth and chroma. Mode switches do not change them;
    /// carried so the stream-shape cap can be recomputed for a new geometry.
    pub(super) bit_depth: u8,
    pub(super) chroma_format: u8,
    /// Host marks idle-keepalive repeats (`USER_FLAG_REPEAT` / Welcome
    /// [`crate::quic::HOST_CAP2_REPEAT_MARK`]). Older hosts are
    /// [`crate::abr::WindowActivity::Unmarked`].
    pub(super) marks_repeats: bool,
    /// Audio-plane wire reservation. Added to window `actual` so the
    /// controller's domain matches the budget its targets are in.
    pub(super) audio_reserved_kbps: u32,
    /// Mode+codec ceiling ([`crate::abr::stream_ceiling_kbps`]). Holds the
    /// probe-measured link ceiling; recomputed on an accepted mode switch.
    pub(super) stream_cap_kbps: u32,
    /// Negotiated refresh. ABR sizes host-encode thresholds in these frame
    /// budgets ([`crate::abr::BitrateController::set_frame_budget`]).
    pub(super) refresh_hz: u32,
    /// Accepted mode, written by the control task. Read when `mode_gen`
    /// moves so the frame budget follows the new refresh.
    pub(super) mode_slot: Arc<Mutex<crate::config::Mode>>,
}

impl DataPump {
    pub(super) fn run(self) {
        let DataPump {
            mut session,
            frames,
            ctrl_tx,
            shutdown: pump_shutdown,
            probe: pump_probe,
            hot_tids: pump_hot_tids,
            clock_offset: pump_clock_offset,
            clock_gen: pump_clock_gen,
            decode_lat: pump_decode_lat,
            encode_lat: pump_encode_lat,
            mode_gen: pump_mode_gen,
            frames_dropped,
            fec_recovered,
            unsustainable_pin_kbps,
            bitrate_ack,
            recovery_kf: pump_recovery_kf,
            pipeline_gap: pump_pipeline_gap,
            bitrate_kbps,
            resolved_bitrate_kbps,
            negotiated_codec,
            bit_depth,
            chroma_format,
            marks_repeats,
            audio_reserved_kbps,
            stream_cap_kbps,
            refresh_hz,
            mode_slot: pump_mode_slot,
        } = self;
        pin_thread_user_interactive(); // frame channel → user-interactive video pump
        register_hot_tid(&pump_hot_tids); // UDP receive + FEC reassembly
                                          // Adaptive-FEC loss window. FLAG_PROBE filler would skew it, so
                                          // reports are suppressed for the whole speed-test burst.
        let mut last_report = Instant::now();
        // DeliveryReport: every window while packets_received is 0, once
        // more when the first packets land, then never. Older hosts log
        // each unknown control message.
        let mut delivery_confirmed = false;
        let (
            mut last_recovered,
            mut last_late,
            mut last_received,
            mut last_dropped,
            mut last_bytes,
        ) = (0u64, 0u64, 0u64, 0u64, 0u64);
        // PUNKTFUNK_PERF: recv/decrypt/reassemble split plus AU inter-arrival
        // jitter. Jump-to-live only fires after the stream is already behind.
        let pump_perf_on = std::env::var("PUNKTFUNK_PERF").is_ok_and(|v| v != "0");
        let mut arrivals_us: Vec<u32> = Vec::new();
        let mut last_arrival: Option<Instant> = None;
        // ABR: Automatic (`bitrate_kbps == 0`) and a non-zero Welcome echo.
        // Old host echoes 0 → controller stays off. PyroWave pins the rate
        // (hard per-frame CBR — AIMD and the climb probe stay off).
        let rate_pinned = negotiated_codec == crate::quic::CODEC_PYROWAVE;
        // All-intra: no reference chain, so the channel drains to newest
        // (`FrameChannel::set_all_intra`) instead of strict FIFO.
        frames.set_all_intra(negotiated_codec == crate::quic::CODEC_PYROWAVE);
        let mut abr = BitrateController::new(if bitrate_kbps == 0 && !rate_pinned {
            resolved_bitrate_kbps
        } else {
            0
        });
        // Bound the probe by stream shape, not raw link capacity. A fat LAN
        // otherwise licenses rates no inter-coded stream can use.
        abr.set_stream_cap(stream_cap_kbps);
        // Encode thresholds in this session's frame budgets, not the 120 Hz
        // durations they were calibrated at. 60 Hz would take SEVERE ×0.7
        // on an ordinary one-frame encode hiccup.
        abr.set_frame_budget(refresh_hz);
        // Startup capacity probe (Automatic): one burst after video flows.
        // Ceiling = delivered × 0.7. Target is `2 × stream_cap` (need
        // delivered ≥ cap × 1.43; `set_ceiling` clamps to the stream cap).
        // `PUNKTFUNK_ABR_PROBE=0` opts out; `_KBPS` overrides the target.
        let capacity_probe_kbps: u32 = std::env::var("PUNKTFUNK_ABR_PROBE_KBPS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|&v| v > 0)
            .unwrap_or_else(|| probe_target_kbps(stream_cap_kbps));
        const CAPACITY_PROBE_MS: u32 = 800;
        const CAPACITY_PROBE_DELAY: Duration = Duration::from_secs(2);
        // Burst aftermath: queue + QUIC loss-recovery sit between host
        // "complete" and our receipt. A late result is discarded.
        const CAPACITY_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
        // 8 windows = 6 s: a probe freeze's keyframe asks (one per 100 ms on
        // webOS) plus a lost IDR's retry.
        const PROBE_AFTERMATH_WINDOWS: u32 = 8;
        let mut capacity_probe_at: Option<Instant> = (bitrate_kbps == 0
            && !rate_pinned
            && resolved_bitrate_kbps > 0
            && std::env::var("PUNKTFUNK_ABR_PROBE").map_or(true, |v| v != "0"))
        .then(|| Instant::now() + CAPACITY_PROBE_DELAY);
        let mut capacity_probe_deadline: Option<Instant> = None;
        // Leading/trailing edge of any probe (startup or embedder). An
        // unanswered request must not wedge the report tick forever.
        let mut was_probing = false;
        // `frames_completed` at burst start: "did any frame survive", not
        // "has one ever arrived".
        let mut frames_at_probe_start: u64 = 0;
        // Discard this window's LossReport / ABR / standing-latency close.
        // Probe tail (reassembler still aging FLAG_PROBE as drops) and a
        // host pipeline rebuild both describe something other than the
        // link; one bogus congestion verdict ends slow start for good.
        let mut discard_abr_window = false;
        // Windows whose keyframe asks still belong to the burst
        // ([`probe_aftermath`]).
        let mut probe_aftermath_left: u32 = 0;
        let mut probe_watchdog: Option<Instant> = None;
        let (mut owd_sum_ns, mut owd_frames) = (0i128, 0u32);
        // Completed video AUs vs host-marked idle repeats. Meaningful only
        // when `marks_repeats`.
        let (mut au_frames, mut au_repeats) = (0u32, 0u32);
        let mut flush_in_window = false;
        // Jump-to-live: clock-based over-bound run (`stale_since`, needs
        // skew handshake), clock-free queue run (`standing_since`), shared
        // cooldown. Wall-clock, not frame counts — fps must not scale it.
        let mut stale_since: Option<Instant> = None;
        let mut standing_since: Option<Instant> = None;
        let mut last_flush: Option<Instant> = None;
        // Consecutive clock-triggered flushes that found no local backlog.
        // `NOOP_CLOCK_FLUSHES_TO_DISARM` turns the clock detector off until
        // a re-sync (`pump_clock_gen`). First no-op also asks for re-sync.
        let mut noop_clock_flushes: u32 = 0;
        let mut clock_detector_armed = true;
        // Flushes that found a real backlog. Under a pinned rate nothing
        // else can lower the load, so the count is the "cannot keep up" fact.
        let mut real_sheds: u32 = 0;
        // AUs dropped while nothing popped the channel (embedder decoder not
        // started yet). The first pop after one owes the host a keyframe.
        let mut unconsumed_aus: u64 = 0;
        let mut resync_wanted = false;
        let mut seen_clock_gen = pump_clock_gen.load(Ordering::Relaxed);
        let mut seen_mode_gen = pump_mode_gen.load(Ordering::Relaxed);
        // Standing-latency bleed: loss-free OWD elevation the two jump
        // detectors tolerate (< QUEUE_HIGH, < FLUSH_LATENCY). Otherwise it
        // reads as permanent extra network latency.
        let mut standing_lat = StandingLatency::new();
        // A hole's two causes told apart: silence at the socket vs. this thread away from it.
        let mut rx_gap = super::rx_gap::RxGap::new(Instant::now());
        let mut ingress_since = (Instant::now(), session.stats());
        while !pump_shutdown.load(Ordering::SeqCst) {
            // Reloaded every iteration so a mid-stream re-sync hits the
            // next frame's latency math.
            let clock_offset_ns = pump_clock_offset.load(Ordering::Relaxed);
            // Re-sync invalidates the staleness run under the old offset.
            let clock_gen = pump_clock_gen.load(Ordering::Relaxed);
            if clock_gen != seen_clock_gen {
                seen_clock_gen = clock_gen;
                stale_since = None;
                noop_clock_flushes = 0;
                // Every OWD reading shifted with the offset; the old floor
                // is meaningless. A stale offset that WAS the elevation
                // is fixed here.
                standing_lat.rebase();
                if !clock_detector_armed {
                    clock_detector_armed = true;
                    tracing::info!("clock re-sync applied — clock-based jump-to-live re-armed");
                }
            }
            // Drain here, not at the report tick, so the in-flight window
            // (the one the rebuild corrupted) is the one we can still drop.
            // A gap that straddled a boundary already fed the previous
            // window; holding every window back would be a permanent lag.
            if let Some(gap_ms) = take_pipeline_gap(&pump_pipeline_gap) {
                discard_abr_window = true;
                tracing::debug!(
                    gap_ms,
                    window_ms = last_report.elapsed().as_millis() as u64,
                    "host pipeline gap — the report window in flight is discarded"
                );
            }
            // Mirror drop/FEC counters every iteration, not only on a
            // produced frame — a total-loss drought completes no AU.
            let st = session.stats();
            if let Some(g) = rx_gap.observe(Instant::now(), st.packets_received) {
                tracing::warn!(
                    silence_ms = g.silence_ms,
                    unpolled_ms = g.unpolled_ms,
                    burst = g.burst,
                    "receive gap — silence_ms: no datagram reached this socket; unpolled_ms: \
                     this thread's longest absence from the socket meanwhile. Near-equal = \
                     this client stalled; unpolled small = nothing arrived, the path or the host"
                );
            }
            let elapsed = ingress_since.0.elapsed();
            if elapsed >= super::rx_gap::IngressWindow::PERIOD {
                let w = super::rx_gap::IngressWindow::between(&ingress_since.1, &st, elapsed);
                tracing::info!(
                    packets = w.packets,
                    video_kbps = w.video_kbps,
                    fec_repaired = w.fec_repaired,
                    frames_dropped = w.frames_dropped,
                    rejected = w.rejected,
                    max_gap_ms = rx_gap.take_max_silence().as_millis() as u64,
                    "wire ingress"
                );
                ingress_since = (Instant::now(), st);
            }
            frames_dropped.store(st.frames_dropped, Ordering::Relaxed);
            fec_recovered.store(st.fec_recovered_shards, Ordering::Relaxed);
            let probe_active = {
                let mut p = pump_probe.lock().unwrap();
                if p.active && !p.done {
                    // First mirror tick: zero arrival stamps before the
                    // burst can claim them. ProbeRequest is still local, so
                    // the reset cannot race a probe packet.
                    let arming = p.base_bytes.is_none();
                    if arming {
                        session.reset_probe_arrivals();
                    }
                    p.rx_packets_now = st.probe_packets_received;
                    p.rx_bytes_now = st.probe_bytes_received;
                    (p.first_arrival_ns, p.last_arrival_ns) = if arming {
                        (0, 0)
                    } else {
                        (st.probe_first_arrival_ns, st.probe_last_arrival_ns)
                    };
                    p.base_packets.get_or_insert(st.probe_packets_received);
                    p.base_bytes.get_or_insert(st.probe_bytes_received);
                }
                p.active && !p.done
            };
            // Probe ended: rebase every window anchor past the burst.
            // FLAG_PROBE landed in packet/byte counters but never the
            // decoder; without this the first post-burst window poisons
            // proven-throughput (monotone, never decays) and loss_ppm.
            if was_probing && !probe_active {
                last_recovered = st.fec_recovered_shards;
                last_late = st.fec_late_shards;
                last_received = st.packets_received;
                last_dropped = st.frames_dropped;
                last_bytes = wire_bytes(&st);
                last_report = Instant::now();
                discard_abr_window = true;
                probe_aftermath_left = PROBE_AFTERMATH_WINDOWS;
                flush_in_window = false;
                // Burst may have taken the keyframe with it. Compare
                // against the count snapshotted at the leading edge, not
                // 0: that also catches a mid-session embedder speed test.
                // One request per probe, via the control-task coalescer.
                if st.frames_completed == frames_at_probe_start {
                    let _ = ctrl_tx.try_send(CtrlRequest::Keyframe);
                    tracing::warn!(
                        "no frame survived the capacity probe — requested a keyframe to re-anchor"
                    );
                }
            }
            // Leading edge of any probe: an old host that ignores
            // ProbeRequest must not latch `active` and mute reports.
            if !was_probing && probe_active {
                let burst = Duration::from_millis(pump_probe.lock().unwrap().duration_ms as u64);
                probe_watchdog = Some(Instant::now() + burst + CAPACITY_PROBE_TIMEOUT);
                frames_at_probe_start = st.frames_completed;
            }
            if !probe_active {
                probe_watchdog = None;
            } else if let Some(deadline) = probe_watchdog {
                if Instant::now() >= deadline {
                    probe_watchdog = None;
                    pump_probe.lock().unwrap().active = false;
                    tracing::warn!(
                        "speed-test probe unanswered — clearing it so loss reports and ABR resume"
                    );
                }
            }
            was_probing = probe_active;
            // Startup probe once video flows. One ProbeState, no
            // correlation id — do not clobber an embedder speed test.
            // "Settled" is a completed frame, not the 2 s timer: a slow
            // host bring-up is still emitting its first IDR.
            if capacity_probe_at.is_some_and(|at| Instant::now() >= at)
                && (probe_active || st.frames_completed == 0)
            {
                capacity_probe_at = Some(Instant::now() + CAPACITY_PROBE_DELAY);
            } else if capacity_probe_at.is_some_and(|at| Instant::now() >= at) {
                capacity_probe_at = None;
                *pump_probe.lock().unwrap() = ProbeState {
                    active: true,
                    duration_ms: CAPACITY_PROBE_MS,
                    ..Default::default()
                };
                if ctrl_tx
                    .try_send(CtrlRequest::Probe(ProbeRequest {
                        target_kbps: capacity_probe_kbps,
                        duration_ms: CAPACITY_PROBE_MS,
                    }))
                    .is_ok()
                {
                    capacity_probe_deadline = Some(Instant::now() + CAPACITY_PROBE_TIMEOUT);
                    tracing::info!(
                        target_kbps = capacity_probe_kbps,
                        duration_ms = CAPACITY_PROBE_MS,
                        "adaptive bitrate: startup link-capacity probe"
                    );
                } else {
                    pump_probe.lock().unwrap().active = false; // ctrl queue full — skip
                }
            }
            if let Some(deadline) = capacity_probe_deadline {
                let mut p = pump_probe.lock().unwrap();
                if p.done {
                    capacity_probe_deadline = None;
                    // All-zero reply = decline: keep the negotiated ceiling.
                    // Else delivered × 0.7 over the CLIENT receive interval
                    // (the host send window closes while the bottleneck
                    // queue is still draining; that duration overstates).
                    if p.host_duration_ms > 0 && p.delivered_bytes > 0 {
                        let window_ms = p.throughput_window_ms(p.delivered_packets);
                        let delivered_kbps =
                            (p.delivered_bytes.saturating_mul(8) / window_ms.max(1) as u64) as u32;
                        let ceiling = delivered_kbps.saturating_mul(7) / 10;
                        abr.set_ceiling(ceiling);
                        tracing::info!(
                            delivered_kbps,
                            ceiling_kbps = ceiling,
                            client_interval_ms = p.client_interval_ms,
                            host_duration_ms = p.host_duration_ms,
                            "adaptive bitrate: link-capacity probe done — climb ceiling set"
                        );
                    } else {
                        tracing::info!(
                            "adaptive bitrate: capacity probe declined — keeping negotiated ceiling"
                        );
                    }
                    // Rebase the ABR byte anchor past the burst. `wire_bytes`
                    // already nets filler; this skips video that landed under
                    // a suppressed report tick (else a long span / one window).
                    last_bytes = wire_bytes(&st);
                } else if Instant::now() >= deadline {
                    // Host never answered: clear stuck-active so LossReports
                    // resume. Keep the negotiated ceiling.
                    p.active = false;
                    capacity_probe_deadline = None;
                    tracing::info!(
                        "adaptive bitrate: capacity probe timed out (old host?) — keeping negotiated ceiling"
                    );
                }
            }
            if !probe_active && last_report.elapsed() >= ADAPT_REPORT_INTERVAL {
                // No-op clock flush suspected a wall-clock step: re-sync
                // once. The 60 s periodic covers everything else.
                if resync_wanted {
                    resync_wanted = false;
                    let _ = ctrl_tx.try_send(CtrlRequest::ClockResync);
                }
                // All-intra drain-to-newest skips are not losses (the wire
                // delivered them). Debug only — do not alarm OSD loss.
                let skipped = frames.take_skipped();
                if skipped > 0 {
                    tracing::debug!(skipped, "all-intra frame channel drained to newest");
                }
                let window_dropped = st.frames_dropped.wrapping_sub(last_dropped);
                // Always drain so a discard window cannot leak its
                // count into the next one.
                let recovery_kf_asked = pump_recovery_kf.swap(0, Ordering::Relaxed);
                let discard = std::mem::take(&mut discard_abr_window);
                // The forced tail window neither spends nor ends the
                // aftermath: its asks may not have surfaced yet.
                let recovery_kf_reqs = if !discard
                    && probe_aftermath(&mut probe_aftermath_left, recovery_kf_asked > 0)
                {
                    tracing::debug!(
                        recovery_kf = recovery_kf_asked,
                        "keyframe asks in the probe's aftermath — not judged as congestion"
                    );
                    0
                } else {
                    recovery_kf_asked
                };
                let loss_ppm = window_loss_ppm(
                    st.fec_recovered_shards.wrapping_sub(last_recovered),
                    st.fec_late_shards.wrapping_sub(last_late),
                    st.packets_received.wrapping_sub(last_received),
                );
                if discard {
                    // LossReport goes with the window: probe tail would
                    // spike host FEC off deliberate overload; a rebuild
                    // window has a near-zero denominator.
                    tracing::debug!(
                        loss_ppm,
                        window_dropped,
                        "discarding this ABR window (probe tail or a host pipeline gap)"
                    );
                } else {
                    let _ = ctrl_tx.try_send(CtrlRequest::Loss(LossReport { loss_ppm }));
                    // DeliveryReport rides the loss report so `loss_ppm = 0`
                    // is readable (flawless vs delivering nothing). Session
                    // total, same arm: a discarded window stays silent.
                    // Cadence is in [`should_report_delivery`].
                    if should_report_delivery(st.packets_received, &mut delivery_confirmed) {
                        let _ = ctrl_tx.try_send(CtrlRequest::Delivery(DeliveryReport {
                            packets_received: st.packets_received,
                        }));
                    }
                }
                // Standing-latency window close. Escalation: re-sync (stale
                // offset), then bleed (flush+keyframe), then disarm (path
                // latency changed). A discard window is NOT loss-free —
                // no action off probe residue.
                match standing_lat.on_window(!discard && loss_ppm == 0 && window_dropped == 0) {
                    StandingLatAction::None => {}
                    StandingLatAction::Resync { above_ms } => {
                        tracing::info!(
                            above_ms,
                            "standing latency above the session floor with zero loss — \
                             requesting a clock re-sync first (a stale offset reads exactly \
                             like this)"
                        );
                        let _ = ctrl_tx.try_send(CtrlRequest::ClockResync);
                    }
                    StandingLatAction::Bleed { above_ms } => {
                        // Shares the jump-to-live cooldown. An unexecuted
                        // bleed re-arms as the detector's run rebuilds.
                        if last_flush.is_none_or(|t| t.elapsed() >= FLUSH_COOLDOWN) {
                            last_flush = Some(Instant::now());
                            // Not `flush_in_window`: that is ABR SEVERE
                            // (immediate ×0.7). Bleed fires after ~6 clean
                            // windows with a sub-25 ms elevation the
                            // controller already scores as fine.
                            let flushed = session.flush_backlog().unwrap_or(0);
                            let dropped = frames.clear();
                            let _ = ctrl_tx.try_send(CtrlRequest::Keyframe);
                            standing_lat.bled();
                            tracing::warn!(
                                above_ms,
                                flushed_datagrams = flushed,
                                dropped_frames = dropped,
                                "standing latency survived a clock re-sync — bled the local \
                                 backlog (flush + keyframe)"
                            );
                        }
                    }
                    StandingLatAction::Disarm { above_ms } => {
                        tracing::warn!(
                            above_ms,
                            "standing latency persists after a re-sync and every bleed — not \
                             local, not clock; the path latency changed. Leaving it be \
                             (reconnect re-baselines)"
                        );
                    }
                }
                let mg = pump_mode_gen.load(Ordering::Relaxed);
                if mg != seen_mode_gen {
                    seen_mode_gen = mg;
                    abr.on_mode_switch();
                    let m = *pump_mode_slot.lock().unwrap();
                    // Frame budget is a mode property: refresh changes
                    // what one frame of encode time costs.
                    abr.set_frame_budget(m.refresh_hz);
                    // Stream-shape cap too. `set_stream_cap` also rebinds
                    // an already-learned ceiling downward for the new
                    // geometry.
                    abr.set_stream_cap(crate::abr::stream_ceiling_kbps(
                        m.width,
                        m.height,
                        m.refresh_hz,
                        negotiated_codec,
                        bit_depth,
                        chroma_format,
                    ));
                }
                for acked in bitrate_ack.lock().unwrap().drain(..) {
                    abr.on_ack(acked);
                }
                let owd_mean_us =
                    (owd_frames > 0).then(|| (owd_sum_ns / owd_frames as i128 / 1000) as i64);
                (owd_sum_ns, owd_frames) = (0, 0);
                // Active = new content this window. Empty ≠ unmarked: no AU
                // is not an older host, and cannot prove repeat-only idle.
                let activity = abr_window_activity(marks_repeats, au_frames, au_repeats);
                (au_frames, au_repeats) = (0, 0);
                // Drain even when ABR is off so the accumulator stays
                // bounded. `None` = nothing reported this window.
                let decode_mean_us = {
                    let mut acc = pump_decode_lat.lock().unwrap();
                    let (sum, count) = (acc.sum_us, acc.count);
                    *acc = DecodeLatAcc::default();
                    (count > 0).then(|| (sum / count as u64) as i64)
                };
                // Host-encode window (0xCF `encode_us`). `None` on an
                // old host that does not send stage timings.
                let encode_mean_us = {
                    let mut acc = pump_encode_lat.lock().unwrap();
                    let (sum, count) = (acc.sum_us, acc.count);
                    *acc = Default::default();
                    (count > 0).then(|| (sum / count as u64) as i64)
                };
                // Wire throughput vs target: headers, seals, FEC parity
                // included (they spend the budget), minus probe filler,
                // plus the audio reservation (spent whether video flows).
                let window_ms = last_report.elapsed().as_millis().max(1) as u64;
                let actual_kbps = ((wire_bytes(&st).wrapping_sub(last_bytes).saturating_mul(8)
                    / window_ms) as u32)
                    .saturating_add(audio_reserved_kbps);
                // Discard window: signals are probe-tail residue. One
                // congestion verdict here ends slow start for good.
                let verdict = if discard {
                    None
                } else {
                    abr.on_window(
                        Instant::now(),
                        window_dropped,
                        loss_ppm,
                        owd_mean_us,
                        decode_mean_us,
                        encode_mean_us,
                        actual_kbps,
                        flush_in_window,
                        recovery_kf_reqs,
                        activity,
                    )
                };
                if let Some(kbps) = verdict {
                    // Log window signals with the decision so decode-/
                    // encode-driven retargets are separable from network.
                    tracing::info!(
                        kbps,
                        loss_ppm,
                        owd_mean_us = owd_mean_us.unwrap_or(-1),
                        decode_mean_us = decode_mean_us.unwrap_or(-1),
                        encode_mean_us = encode_mean_us.unwrap_or(-1),
                        actual_kbps,
                        flushed = flush_in_window,
                        recovery_kf = recovery_kf_reqs,
                        "adaptive bitrate: requesting encoder re-target"
                    );
                    if ctrl_tx.try_send(CtrlRequest::SetBitrate(kbps)).is_err() {
                        // Never reached the control task. Three of these
                        // retire the controller as "the host never acked".
                        abr.on_request_dropped();
                        tracing::warn!(
                            kbps,
                            "adaptive bitrate: control queue full — re-target dropped"
                        );
                    }
                }
                flush_in_window = false;
                last_report = Instant::now();
                last_recovered = st.fec_recovered_shards;
                last_late = st.fec_late_shards;
                last_received = st.packets_received;
                last_dropped = st.frames_dropped;
                last_bytes = wire_bytes(&st);
                if pump_perf_on {
                    if let Some(p) = session.take_pump_perf() {
                        let per_pkt_ns = |ns: u64| ns.checked_div(p.packets).unwrap_or(0);
                        tracing::info!(
                            recv_ms = p.recv_ns / 1_000_000,
                            decrypt_ms = p.decrypt_ns / 1_000_000,
                            reasm_ms = p.reasm_ns / 1_000_000,
                            packets = p.packets,
                            batches = p.batches,
                            pkts_per_batch = p.packets.checked_div(p.batches).unwrap_or(0),
                            decrypt_ns_pkt = per_pkt_ns(p.decrypt_ns),
                            reasm_ns_pkt = per_pkt_ns(p.reasm_ns),
                            "pump stage split (window)"
                        );
                    }
                    // Inter-arrival jitter. `late` = gaps over 2× the
                    // window median (a frame arrived visibly off-beat).
                    if arrivals_us.len() >= 8 {
                        arrivals_us.sort_unstable();
                        let pct = |q: usize| arrivals_us[(arrivals_us.len() - 1) * q / 100];
                        let (p50, p95) = (pct(50), pct(95));
                        let late = arrivals_us.iter().filter(|&&d| d > p50 * 2).count();
                        tracing::info!(
                            frames = arrivals_us.len() + 1,
                            arrival_p50_us = p50,
                            arrival_p95_us = p95,
                            arrival_max_us = arrivals_us.last().copied().unwrap_or(0),
                            late,
                            "frame inter-arrival jitter (window)"
                        );
                    }
                    arrivals_us.clear();
                }
            }
            match session.poll_frame() {
                Ok(frame) => {
                    if frame.flags & FLAG_PROBE as u32 != 0 {
                        continue; // speed-test filler, not video — measured via the counters above
                    }
                    // Prefix parts are not AU arrivals. Inter-arrival,
                    // OWD, and the clock detector are per-AU; parts
                    // would bias OWD low and reset the staleness run.
                    let is_au = frame.complete;
                    if is_au {
                        // Repeats are the host's idle keepalive, not
                        // new content.
                        au_frames = au_frames.saturating_add(1);
                        if frame.flags & crate::packet::USER_FLAG_REPEAT != 0 {
                            au_repeats = au_repeats.saturating_add(1);
                        }
                    }
                    if pump_perf_on && is_au {
                        let now = Instant::now();
                        if let Some(prev) = last_arrival.replace(now) {
                            // 4096 ≈ 17 s at 240 fps — a stuck window
                            // cannot grow it unbounded.
                            if arrivals_us.len() < 4096 {
                                arrivals_us
                                    .push((now - prev).as_micros().min(u32::MAX as u128) as u32);
                            }
                        }
                    }
                    // No decoder yet (the embedder starts it on its stream
                    // view; a console launch hold delays that up to 15 s).
                    // Queued AUs would be reference-broken by then, and a
                    // queue nobody drains is not link distress — so no push,
                    // no detector, and one keyframe once something pops.
                    if !frames.consumer_seen() {
                        unconsumed_aus += u64::from(is_au);
                        stale_since = None;
                        standing_since = None;
                        continue;
                    }
                    if unconsumed_aus > 0 {
                        tracing::info!(
                            dropped_aus = unconsumed_aus,
                            "decoder attached after the stream started — asking for a keyframe"
                        );
                        unconsumed_aus = 0;
                        let _ = ctrl_tx.try_send(CtrlRequest::Keyframe);
                    }
                    // Jump-to-live. In-order consume never catches up;
                    // infinite GOP cannot drop a frame. Clock: > FLUSH_LATENCY
                    // for FLUSH_AFTER. Queue: ≥ QUEUE_HIGH for STANDING_TIME
                    // (still high at the trip). Both gated by FLUSH_COOLDOWN.
                    if probe_active {
                        // Probe measures a saturated queue; a primed run
                        // would fire the moment the burst ended.
                        stale_since = None;
                        standing_since = None;
                    } else {
                        let lat_ns = if clock_offset_ns != 0 && is_au {
                            now_realtime_ns() + clock_offset_ns as i128 - frame.pts_ns as i128
                        } else {
                            0
                        };
                        // Mean capture→received delay. Rising delay under
                        // zero loss is queue growth — the pre-loss signal.
                        if clock_offset_ns != 0 && lat_ns > 0 {
                            owd_sum_ns += lat_ns;
                            owd_frames += 1;
                            // Window MINIMUM, not mean: a standing state
                            // elevates the floor. 10 s clamp matches hn stats.
                            if lat_ns < 10_000_000_000 {
                                standing_lat.note_frame(lat_ns);
                            }
                        }
                        if clock_detector_armed
                            && clock_offset_ns != 0
                            && lat_ns > FLUSH_LATENCY.as_nanos() as i128
                        {
                            stale_since.get_or_insert_with(Instant::now);
                        } else if is_au {
                            stale_since = None;
                        }
                        let depth = frames.depth();
                        if depth >= QUEUE_HIGH {
                            standing_since.get_or_insert_with(Instant::now);
                        } else if depth <= QUEUE_LOW {
                            standing_since = None;
                        }
                        // Still high NOW: a run that started ≥ high but is
                        // in the hysteresis band (clump mid-drain) must
                        // not fire on elapsed time alone.
                        let clock_behind = stale_since.is_some_and(|t| t.elapsed() >= FLUSH_AFTER);
                        let queue_behind = depth >= QUEUE_HIGH
                            && standing_since.is_some_and(|t| t.elapsed() >= STANDING_TIME);
                        if (clock_behind || queue_behind)
                            && last_flush.is_none_or(|t| t.elapsed() >= FLUSH_COOLDOWN)
                        {
                            stale_since = None;
                            standing_since = None;
                            last_flush = Some(Instant::now());
                            flush_in_window = true; // ABR SEVERE: link cannot hold the rate
                            let flushed = session.flush_backlog().unwrap_or(0);
                            let dropped = frames.clear();
                            let _ = ctrl_tx.try_send(CtrlRequest::Keyframe);
                            tracing::warn!(
                                behind_ms = if clock_behind { lat_ns / 1_000_000 } else { -1 },
                                queue_depth = depth,
                                flushed_datagrams = flushed,
                                dropped_frames = dropped,
                                "receive backlog stopped draining — jumped to live (flush + keyframe)"
                            );
                            // Clock-only flush with no local backlog is a
                            // false behind (clock step / upstream queue).
                            // Two in a row disarm; the queue detector stays.
                            if clock_behind
                                && !queue_behind
                                && flushed < NOOP_FLUSH_DATAGRAMS
                                && dropped == 0
                            {
                                noop_clock_flushes += 1;
                                if noop_clock_flushes == 1 {
                                    // First no-op: ask for an immediate
                                    // re-sync. Applied, it re-arms before
                                    // the disarm below triggers.
                                    resync_wanted = true;
                                }
                                if noop_clock_flushes >= NOOP_CLOCK_FLUSHES_TO_DISARM {
                                    clock_detector_armed = false;
                                    tracing::warn!(
                                        "clock-based jump-to-live disarmed — its flushes found no \
                                         local backlog (clock step or upstream queueing suspected); \
                                         the queue-depth detector stays armed"
                                    );
                                }
                            } else {
                                noop_clock_flushes = 0;
                                real_sheds += 1;
                                if bitrate_kbps != 0 && real_sheds == PIN_SHEDS_TO_WARN {
                                    unsustainable_pin_kbps.store(bitrate_kbps, Ordering::Relaxed);
                                    tracing::warn!(
                                        pinned_kbps = bitrate_kbps,
                                        sheds = real_sheds,
                                        "pinned bitrate above what this client sustains — the \
                                         receive backlog keeps being shed"
                                    );
                                }
                            }
                            continue; // this frame is the stale past
                        }
                    }
                    frames.push(frame);
                }
                Err(PunktfunkError::NoFrame) => {
                    std::thread::sleep(Duration::from_micros(300));
                }
                Err(_) => break,
            }
        }
        // Wake a consumer blocked in `next_frame` with Closed, not a timeout.
        frames.close();
    }
}

/// Drain the host's pending pipeline gap. `Some(gap_ms)` = the in-flight
/// report window must be discarded. Swap-to-zero so one announcement
/// cannot keep poisoning later windows.
fn take_pipeline_gap(slot: &AtomicU32) -> Option<u32> {
    match slot.swap(0, Ordering::Relaxed) {
        0 => None,
        gap_ms => Some(gap_ms),
    }
}

/// Whether this window owes the host a [`DeliveryReport`].
///
/// Every window while `packets_received` is 0 (the host escalates on
/// that), then once when the first packets land, then silence. Older
/// hosts log every unknown control message.
fn should_report_delivery(packets_received: u64, confirmed: &mut bool) -> bool {
    let owed = packets_received == 0 || !*confirmed;
    *confirmed = packets_received > 0;
    owed
}

/// Capacity-probe burst target in kbps. `PUNKTFUNK_ABR_PROBE_KBPS`
/// overrides. `set_ceiling` clamps to the stream cap, so bits above
/// `cap / 0.7` are discarded; ×2 clears the 1.43× bar with margin.
///
/// `u32::MAX` (a mode [`crate::abr::stream_ceiling_kbps`] declines to size)
/// keeps 2 Gbps — also the hard ceiling; this can only lower the target.
fn probe_target_kbps(stream_cap_kbps: u32) -> u32 {
    stream_cap_kbps.saturating_mul(2).min(2_000_000)
}

/// Whether this window's keyframe asks still belong to the capacity probe.
///
/// Video shares the link with the burst, so an overdriven link drops frames
/// beside it and the client asks for keyframes until one lands — past the
/// discarded tail window. Two asks in a window end slow start and four cut the
/// rate, yet they say nothing about the link after the burst. Only the asks are
/// disowned: drops, loss and a flush in the same window are still judged, which
/// is what keeps a link the start rate overloads from hiding here. Each window
/// with asks spends one of `windows_left`; the first without ends the aftermath.
fn probe_aftermath(windows_left: &mut u32, asked: bool) -> bool {
    if *windows_left == 0 {
        return false;
    }
    if asked {
        *windows_left -= 1;
    } else {
        *windows_left = 0;
    }
    asked
}

/// Classify this window's new-content evidence for ABR.
///
/// No arrivals are [`WindowActivity::Empty`]: quiet like idle, not an
/// older-host unmarked window. Repeat-only (every arrived AU a host-marked
/// repeat) is [`WindowActivity::Active`]`(0)` and counts toward re-arm.
/// Arrivals on a host that does not mark repeats are
/// [`WindowActivity::Unmarked`]. The controller never infers stillness
/// from a blackout.
fn abr_window_activity(marks_repeats: bool, frames: u32, repeats: u32) -> WindowActivity {
    if frames == 0 {
        WindowActivity::Empty
    } else if marks_repeats {
        WindowActivity::Active(frames.saturating_sub(repeats))
    } else {
        WindowActivity::Unmarked
    }
}

/// Wire measure: every received media-plane byte (headers, seals, FEC
/// parity spend the budget) minus speed-test filler.
fn wire_bytes(st: &crate::stats::Stats) -> u64 {
    st.bytes_received.wrapping_sub(st.probe_bytes_received)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DeliveryReport: "zero" while true, one confirmation when video
    /// starts, then silence (older hosts warn per unknown message).
    #[test]
    fn the_delivery_count_is_reported_while_zero_then_once_more_and_never_again() {
        let mut confirmed = false;
        for _ in 0..5 {
            assert!(
                should_report_delivery(0, &mut confirmed),
                "a dead data plane must be re-reported every window"
            );
        }
        assert!(should_report_delivery(500, &mut confirmed));
        for n in [900, 1_200, 90_000] {
            assert!(
                !should_report_delivery(n, &mut confirmed),
                "a healthy session must not stream delivery reports"
            );
        }
    }

    /// A session that never receives must never look confirmed.
    #[test]
    fn a_session_that_receives_nothing_never_reports_itself_healthy() {
        let mut confirmed = false;
        for _ in 0..100 {
            assert!(should_report_delivery(0, &mut confirmed));
            assert!(!confirmed);
        }
    }

    /// Burst must prove the stream cap and no more. Above `cap / 0.7`
    /// is discarded by `set_ceiling` (see `abr::tests::the_stream_bound_clamps_a_learned_ceiling_only`).
    #[test]
    fn the_probe_target_proves_the_stream_cap_without_overshooting_it() {
        for (w, h, hz, codec, depth) in [
            (1280, 720, 60, crate::quic::CODEC_HEVC, 8),
            (1920, 1080, 60, crate::quic::CODEC_H264, 8),
            (2560, 1440, 120, crate::quic::CODEC_HEVC, 8),
            (3840, 2160, 120, crate::quic::CODEC_HEVC, 10),
        ] {
            let cap = crate::abr::stream_ceiling_kbps(w, h, hz, codec, depth, 0);
            let target = probe_target_kbps(cap);
            assert!(
                target.saturating_mul(7) / 10 >= cap,
                "{w}x{h}@{hz}: a {target} kbps burst cannot prove a {cap} kbps cap"
            );
            assert!(
                target <= cap.saturating_mul(2),
                "{w}x{h}@{hz}: {target} kbps chases capacity the clamp discards"
            );
        }
        assert_eq!(probe_target_kbps(u32::MAX), 2_000_000);
        assert_eq!(probe_target_kbps(1_500_000), 2_000_000);
    }

    /// The burst's keyframe asks are disowned until a window has none, never
    /// past the budget.
    #[test]
    fn probe_keyframe_asks_are_disowned_until_the_stream_recovers() {
        let mut left = 8;
        assert!(probe_aftermath(&mut left, true));
        assert!(probe_aftermath(&mut left, true));
        assert!(!probe_aftermath(&mut left, false), "no asks ends it");
        assert!(
            !probe_aftermath(&mut left, true),
            "asks after recovery are congestion"
        );

        let mut left = 2;
        assert!(probe_aftermath(&mut left, true));
        assert!(probe_aftermath(&mut left, true));
        assert!(
            !probe_aftermath(&mut left, true),
            "a client that never recovers is judged once the budget is spent"
        );
    }

    #[test]
    fn only_observed_repeats_make_an_idle_abr_window() {
        assert_eq!(abr_window_activity(true, 45, 45), WindowActivity::Active(0));
        assert_eq!(abr_window_activity(true, 45, 40), WindowActivity::Active(5));
        assert_eq!(abr_window_activity(true, 0, 0), WindowActivity::Empty);
        assert_eq!(abr_window_activity(false, 45, 0), WindowActivity::Unmarked);
        assert_eq!(abr_window_activity(false, 0, 0), WindowActivity::Empty);
    }

    #[test]
    fn a_pipeline_gap_is_taken_exactly_once() {
        let slot = AtomicU32::new(0);
        assert_eq!(
            take_pipeline_gap(&slot),
            None,
            "an idle session announces nothing"
        );
        slot.store(401, Ordering::Relaxed);
        assert_eq!(take_pipeline_gap(&slot), Some(401));
        // Drain bounds discard to one window. Silence would look like
        // a clean link to host adaptive FEC.
        assert_eq!(take_pipeline_gap(&slot), None);
    }

    /// Idle client-role loopback. The pump under test is its report tick,
    /// not frames.
    fn idle_client_session() -> (crate::transport::LoopbackTransport, Session) {
        let (host_tp, client_tp) = crate::transport::loopback_pair(0, 0);
        let cfg = crate::config::Config {
            role: crate::config::Role::Client,
            phase: crate::config::ProtocolPhase::P2Punktfunk,
            fec: crate::config::FecConfig {
                scheme: crate::config::FecScheme::Gf16,
                fec_percent: 25,
                max_data_per_block: 32,
            },
            shard_payload: 1024,
            max_frame_bytes: 1 << 20,
            encrypt: false,
            key: crate::crypto::SessionKey::Aes128Gcm([7u8; 16]),
            salt: [1, 2, 3, 4],
            loopback_drop_period: 0,
        };
        // Keep the host end so the link stays whole for the pump's run.
        (host_tp, Session::new(cfg, Box::new(client_tp)).unwrap())
    }

    /// Host-rebuild repair, end to end: a real [`PipelineGap`] on a real
    /// control stream, the control task parks it, the pump discards the
    /// window it landed in.
    ///
    /// Assertions watch the window's LossReport — the discarded window's
    /// only externally visible product on an idle session. A near-zero
    /// denominator would have the host raise FEC against a link that
    /// never dropped. The next window must report: discard is one wide.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn a_host_pipeline_gap_discards_the_report_window_in_flight() {
        let server = crate::quic::endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let client = crate::quic::endpoint::client_insecure().unwrap();
        let accept = tokio::spawn(async move {
            let incoming = server.accept().await.expect("incoming");
            (server, incoming.await.expect("host side connects"))
        });
        let client_conn = client.connect(addr, "punktfunk").unwrap().await.unwrap();
        let (_server_ep, host_conn) = accept.await.unwrap();
        // Host opens the control stream (normally the client does during
        // handshake): this host end only writes, so a client-opened
        // stream would stay invisible.
        let accept_ctrl = tokio::spawn(async move { client_conn.accept_bi().await.unwrap() });
        let (mut host_send, _host_recv) = host_conn.open_bi().await.unwrap();
        io::write_msg(&mut host_send, &crate::quic::RequestKeyframe.encode())
            .await
            .expect("open the stream with a message the client ignores");
        let (ctrl_send, ctrl_recv) = accept_ctrl.await.unwrap();

        let pipeline_gap = Arc::new(AtomicU32::new(0));
        // Hold the sender so the task does not exit on a closed channel.
        let (_task_ctrl_tx, task_ctrl_rx) = tokio::sync::mpsc::channel::<CtrlRequest>(8);
        let (clip_event_tx, _clip_event_rx) = std::sync::mpsc::sync_channel(8);
        let (cursor_shape_tx, _cursor_shape_rx) = std::sync::mpsc::sync_channel(8);
        let (access_tx, _access_rx) = std::sync::mpsc::sync_channel(8);
        tokio::spawn(
            super::super::control_task::ControlTask {
                ctrl_rx: task_ctrl_rx,
                ctrl_send,
                ctrl_recv: io::MsgReader::new(ctrl_recv),
                clock_rtt_ns: None, // no connect handshake ⇒ no re-sync batches to interleave
                mode_slot: Arc::new(Mutex::new(crate::config::Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                })),
                probe: Arc::new(Mutex::new(ProbeState::default())),
                bitrate_ack: Arc::new(Mutex::new(std::collections::VecDeque::new())),
                live_bitrate: Arc::new(AtomicU32::new(0)),
                recovery_kf: Arc::new(AtomicU32::new(0)),
                pipeline_gap: pipeline_gap.clone(),
                clock_offset: Arc::new(std::sync::atomic::AtomicI64::new(0)),
                clock_gen: Arc::new(AtomicU32::new(0)),
                clip_event_tx,
                cursor_shape_tx,
                mode_gen: Arc::new(AtomicU32::new(0)),
                access_grants: Arc::new(AtomicU32::new(0)),
                access_deadline_unix: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                access_tx,
                audio_mute: Arc::new(std::sync::atomic::AtomicU8::new(0)),
                pad_slots: Arc::new(std::sync::atomic::AtomicU16::new(0)),
                launch_outcome: Arc::new(Mutex::new(None)),
            }
            .run(),
        );

        // Explicit bitrate (not Automatic): keep the controller and the
        // startup probe out. The probe would discard a window of its own.
        let (pump_ctrl_tx, mut pump_ctrl_rx) = tokio::sync::mpsc::channel::<CtrlRequest>(8);
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (_host_tp, session) = idle_client_session();
        let pump = DataPump {
            session,
            frames: Arc::new(FrameChannel::new()),
            ctrl_tx: pump_ctrl_tx,
            shutdown: shutdown.clone(),
            probe: Arc::new(Mutex::new(ProbeState::default())),
            hot_tids: Arc::new(Mutex::new(Vec::new())),
            clock_offset: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            clock_gen: Arc::new(AtomicU32::new(0)),
            decode_lat: Arc::new(Mutex::new(DecodeLatAcc::default())),
            encode_lat: Arc::new(Mutex::new(Default::default())),
            mode_gen: Arc::new(AtomicU32::new(0)),
            frames_dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            unsustainable_pin_kbps: Arc::new(AtomicU32::new(0)),
            fec_recovered: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            bitrate_ack: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            recovery_kf: Arc::new(AtomicU32::new(0)),
            pipeline_gap: pipeline_gap.clone(),
            bitrate_kbps: 20_000,
            resolved_bitrate_kbps: 20_000,
            negotiated_codec: crate::quic::CODEC_HEVC,
            bit_depth: 8,
            chroma_format: 0,
            marks_repeats: false,
            audio_reserved_kbps: 256,
            stream_cap_kbps: 100_000,
            refresh_hz: 60,
            mode_slot: Arc::new(Mutex::new(crate::config::Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })),
        };
        let started = Instant::now();
        let pump_thread = std::thread::spawn(move || pump.run());

        // Mid-window, as a rebuild actually lands: 200 ms into 750 ms.
        tokio::time::sleep(Duration::from_millis(200)).await;
        io::write_msg(
            &mut host_send,
            &crate::quic::PipelineGap { gap_ms: 401 }.encode(),
        )
        .await
        .unwrap();

        // Past the first report tick (750 ms), not the second (1500 ms).
        tokio::time::sleep_until(
            tokio::time::Instant::from_std(started) + Duration::from_millis(1_150),
        )
        .await;
        assert!(
            pump_ctrl_rx.try_recv().is_err(),
            "the window the host's rebuild landed in must be discarded, not reported"
        );
        assert_eq!(
            pipeline_gap.load(Ordering::Relaxed),
            0,
            "and the announcement must be drained, so it can't discard a second window"
        );

        // Next window must report. A wedged pump would fail here rather
        // than pass the discard assert above.
        let reported = tokio::time::timeout(Duration::from_millis(1_500), pump_ctrl_rx.recv())
            .await
            .expect("the window after the gap reports on schedule");
        assert!(
            matches!(reported, Some(CtrlRequest::Loss(_))),
            "the window after the gap must produce a loss report — the first of the two requests \
             an idle session makes (the delivery count follows it)"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(1_400),
            "and it must be the SECOND window's report, not a late first"
        );

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        pump_thread.join().unwrap();
    }
}
