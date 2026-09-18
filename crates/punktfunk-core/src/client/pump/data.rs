//! Blocking data-plane pump: poll the session, run Adaptive-FEC / ABR /
//! jump-to-live / standing-latency, and hand frames to the embedder.
//!
//! Dedicated user-interactive thread. Newest-frame drop on embedder lag.
//! [`FLAG_PROBE`] filler never enters the decoder. The ABR window is
//! assembled by [`crate::abr::Driver`]: this file feeds it events and sends
//! what it asks for.

use super::super::*;
use super::*;
use crate::abr::{Action, DriverConfig, ProbeReport};

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
    /// state ([`crate::abr::Driver::on_mode_switch`]).
    pub(super) mode_gen: Arc<AtomicU32>,
    pub(super) frames_dropped: Arc<std::sync::atomic::AtomicU64>,
    pub(super) fec_recovered: Arc<std::sync::atomic::AtomicU64>,
    /// The pinned rate this client could not hold, kbps; `0` until it sheds
    /// its backlog [`PIN_SHEDS_TO_WARN`] times. Embedders show it once.
    pub(super) unsustainable_pin_kbps: Arc<AtomicU32>,
    /// Host `BitrateChanged` acks, drained in arrival order. A queue so a
    /// corrective short retarget cannot be clobbered by a full resolve ack
    /// in the same window (host-cap learning needs two consecutive shorts).
    pub(super) bitrate_ack: Arc<Mutex<AckQueue>>,
    /// Decode-recovery keyframe asks, counted at the control-task send choke.
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
    /// [`crate::quic::HOST_CAP2_REPEAT_MARK`]).
    pub(super) marks_repeats: bool,
    /// Audio-plane wire reservation, spent whether video flows or not.
    pub(super) audio_reserved_kbps: u32,
    /// Mode+codec ceiling ([`crate::abr::stream_ceiling_kbps`]) for the
    /// negotiated geometry; recomputed by the driver on a mode switch.
    pub(super) stream_cap_kbps: u32,
    /// Negotiated refresh, not the request still sitting in `mode_slot`.
    pub(super) refresh_hz: u32,
    /// Accepted mode, written by the control task. Read when `mode_gen`
    /// moves so the driver follows the new geometry.
    pub(super) mode_slot: Arc<Mutex<crate::config::Mode>>,
    /// Published each window from [`crate::abr::Driver::last_cut`].
    pub(super) rate_cut: Arc<std::sync::atomic::AtomicU8>,
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
            rate_cut,
        } = self;
        pin_thread_user_interactive(); // frame channel → user-interactive video pump
        register_hot_tid(&pump_hot_tids); // UDP receive + FEC reassembly
                                          // PUNKTFUNK_PERF: recv/decrypt/reassemble split plus AU inter-arrival
                                          // jitter. Jump-to-live only fires after the stream is already behind.
        let pump_perf_on = std::env::var("PUNKTFUNK_PERF").is_ok_and(|v| v != "0");
        let mut arrivals_us: Vec<u32> = Vec::new();
        let mut last_arrival: Option<Instant> = None;
        // PyroWave pins the rate (hard per-frame CBR), so Automatic never
        // arms: no AIMD, no climb probe.
        let rate_pinned = negotiated_codec == crate::quic::CODEC_PYROWAVE;
        // All-intra: no reference chain, so the channel drains to newest
        // (`FrameChannel::set_all_intra`) instead of strict FIFO.
        frames.set_all_intra(negotiated_codec == crate::quic::CODEC_PYROWAVE);
        // The three environment overrides, read once. Automatic is a session
        // with no embedder rate and a host that echoed one.
        let mut abr = crate::abr::Driver::new(
            DriverConfig {
                start_kbps: if bitrate_kbps == 0 && !rate_pinned {
                    resolved_bitrate_kbps
                } else {
                    0
                },
                ceiling_cap_kbps: env_u32("PUNKTFUNK_ABR_MAX_MBPS")
                    .map(|m| m.saturating_mul(1_000)),
                stream_cap_kbps,
                refresh_hz,
                codec: negotiated_codec,
                bit_depth,
                chroma_format,
                audio_reserved_kbps,
                marks_repeats,
                probe: std::env::var("PUNKTFUNK_ABR_PROBE").map_or(true, |v| v != "0"),
                probe_target_kbps: env_u32("PUNKTFUNK_ABR_PROBE_KBPS"),
            },
            Instant::now(),
        );
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
            if let Some(gap_ms) = take_pipeline_gap(&pump_pipeline_gap) {
                abr.on_pipeline_gap(gap_ms);
            }
            // Mirror drop/FEC counters every iteration, not only on a
            // produced frame — a total-loss drought completes no AU.
            let st = session.stats();
            abr.on_stats(&st);
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
            let (probe_active, probe_duration_ms, probe_report) = {
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
                let report = p.done.then(|| ProbeReport {
                    delivered_bytes: p.delivered_bytes,
                    window_ms: p.throughput_window_ms(p.delivered_packets),
                    host_duration_ms: p.host_duration_ms,
                    client_interval_ms: p.client_interval_ms,
                });
                (p.active && !p.done, p.duration_ms, report)
            };
            abr.on_probe_active(probe_active, probe_duration_ms, Instant::now());
            if let Some(r) = probe_report {
                abr.on_probe_result(r);
            }
            let mg = pump_mode_gen.load(Ordering::Relaxed);
            if mg != seen_mode_gen {
                seen_mode_gen = mg;
                let m = *pump_mode_slot.lock().unwrap();
                abr.on_mode_switch(m.width, m.height, m.refresh_hz);
            }
            for (acked, why) in bitrate_ack.lock().unwrap().drain(..) {
                abr.on_ack(acked, why);
            }
            // Drain even when the controller is off, so the accumulators
            // stay bounded and no count leaks into a later window.
            let (sum, count) = {
                let mut acc = pump_decode_lat.lock().unwrap();
                let taken = (acc.sum_us, acc.count);
                *acc = DecodeLatAcc::default();
                taken
            };
            abr.on_decode_latency(sum, count);
            let (sum, count) = {
                let mut acc = pump_encode_lat.lock().unwrap();
                let taken = (acc.sum_us, acc.count);
                *acc = Default::default();
                taken
            };
            abr.on_encode_latency(sum, count);
            abr.on_keyframe_asks(pump_recovery_kf.swap(0, Ordering::Relaxed));
            let tick = abr.tick(Instant::now());
            for action in tick.actions {
                match action {
                    Action::Loss(loss_ppm) => {
                        let _ = ctrl_tx.try_send(CtrlRequest::Loss(LossReport { loss_ppm }));
                    }
                    Action::Delivery(packets_received) => {
                        let _ = ctrl_tx
                            .try_send(CtrlRequest::Delivery(DeliveryReport { packets_received }));
                    }
                    Action::SetBitrate(kbps) => {
                        if ctrl_tx.try_send(CtrlRequest::SetBitrate(kbps)).is_err() {
                            // Never reached the control task. Three of these
                            // retire the controller as "the host never acked".
                            abr.on_request_dropped(kbps);
                        }
                    }
                    Action::Keyframe => {
                        let _ = ctrl_tx.try_send(CtrlRequest::Keyframe);
                    }
                    Action::Probe {
                        target_kbps,
                        duration_ms,
                    } => {
                        // One ProbeState, no correlation id — do not clobber
                        // an embedder speed test.
                        *pump_probe.lock().unwrap() = ProbeState {
                            active: true,
                            duration_ms,
                            ..Default::default()
                        };
                        if ctrl_tx
                            .try_send(CtrlRequest::Probe(ProbeRequest {
                                target_kbps,
                                duration_ms,
                            }))
                            .is_err()
                        {
                            pump_probe.lock().unwrap().active = false; // ctrl queue full — skip
                            abr.on_probe_dropped();
                        }
                    }
                    Action::AbandonProbe => pump_probe.lock().unwrap().active = false,
                }
            }
            if let Some(window) = tick.window {
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
                // Standing-latency window close. Escalation: re-sync (stale
                // offset), then bleed (flush+keyframe), then disarm (path
                // latency changed).
                match standing_lat.on_window(window.loss_free) {
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
                            // Not a jump-to-live: that is ABR SEVERE
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
                // The overlay names why Automatic sits low; the cut
                // stands until the host grants a climb.
                rate_cut.store(
                    abr.last_cut()
                        .and_then(crate::hud::RateCut::of_reason)
                        .map_or(0, |c| c as u8),
                    Ordering::Relaxed,
                );
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
                        abr.on_au(frame.flags & crate::packet::USER_FLAG_REPEAT != 0);
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
                            abr.on_owd(lat_ns);
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
                            abr.on_flush(); // SEVERE: the link cannot hold the rate
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

/// A positive `u32` from the environment. Unset, zero or garbage is `None`.
fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&v| v > 0)
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

#[cfg(test)]
mod tests {
    use super::*;

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
                bitrate_ack: Arc::new(Mutex::new(AckQueue::new())),
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
            bitrate_ack: Arc::new(Mutex::new(AckQueue::new())),
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
            rate_cut: Arc::new(std::sync::atomic::AtomicU8::new(0)),
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
