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
    pub(super) shared: Arc<ClientShared>,
    pub(super) ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    pub(super) clock_gen: Arc<AtomicU32>,
    /// Host encode-stage window ([`super::super::frame_channel::EncodeLatAcc`]);
    /// fed by the datagram task, not the overlay's lossy `host_timing_tx`.
    pub(super) encode_lat: Arc<Mutex<super::super::frame_channel::EncodeLatAcc>>,
    /// Control-task mode-switch generation. A change resets mode-scoped ABR
    /// state ([`crate::abr::Driver::on_mode_switch`]).
    pub(super) mode_gen: Arc<AtomicU32>,
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
    /// Host serves probe requests during its own bring-up
    /// ([`crate::quic::HOST_CAP2_RAMP`]): the link is measured before the
    /// first frame instead of burst at beside it.
    pub(super) serves_ramp: bool,
    /// Host reads a delivery count every window
    /// ([`crate::quic::HOST_CAP2_DELIVERY`]) to divide a shared path.
    pub(super) reads_delivery: bool,
    /// Audio-plane wire reservation, spent whether video flows or not.
    pub(super) audio_reserved_kbps: u32,
    /// Mode+codec ceiling ([`crate::abr::stream_ceiling_kbps`]) for the
    /// negotiated geometry; recomputed by the driver on a mode switch.
    pub(super) stream_cap_kbps: u32,
    /// Negotiated refresh, not the request still sitting in `shared.mode`.
    pub(super) refresh_hz: u32,
}

/// Closed windows held for an embedder that has not read them. Forty-eight
/// seconds at the report cadence: enough that a client polling once a second
/// never loses one, small enough that one which never polls costs nothing.
pub(crate) const ABR_TRAJECTORY_WINDOWS: usize = 64;

/// What one pump run carries from iteration to iteration.
struct PumpLoop {
    abr: crate::abr::Driver,
    session_start: Instant,
    jump: JumpToLive,
    /// Loss-free OWD elevation the jump detectors tolerate (< QUEUE_HIGH,
    /// < FLUSH_LATENCY). Otherwise it reads as permanent extra latency.
    standing_lat: StandingLatency,
    /// A hole's two causes told apart: silence at the socket vs. this thread away from it.
    rx_gap: super::rx_gap::RxGap,
    ingress_since: (Instant, crate::stats::Stats),
    /// Newest video index handed on; a jump past it names a short frame.
    last_index: Option<u32>,
    /// AUs dropped while nothing popped the channel (embedder decoder not
    /// started yet). The first pop after one owes the host a keyframe.
    unconsumed_aus: u64,
    seen_clock_gen: u32,
    seen_mode_gen: u32,
    /// Epoch of the last frame handed on; a new one may move the mode ([`super::anchor`]).
    last_epoch: Option<u8>,
    /// `PUNKTFUNK_PERF`: recv/decrypt/reassemble split plus AU inter-arrival
    /// jitter. Jump-to-live only fires after the stream is already behind.
    perf: Option<PerfWindow>,
}

impl PumpLoop {
    /// Warn on a receive gap, and log the wire ingress every
    /// [`IngressWindow::PERIOD`](super::rx_gap::IngressWindow::PERIOD).
    fn watch_ingress(&mut self, st: &crate::stats::Stats) {
        if let Some(g) = self.rx_gap.observe(Instant::now(), st.packets_received) {
            tracing::warn!(
                silence_ms = g.silence_ms,
                unpolled_ms = g.unpolled_ms,
                burst = g.burst,
                "receive gap — silence_ms: no datagram reached this socket; unpolled_ms: \
                 this thread's longest absence from the socket meanwhile. Near-equal = \
                 this client stalled; unpolled small = nothing arrived, the path or the host"
            );
        }
        let elapsed = self.ingress_since.0.elapsed();
        if elapsed >= super::rx_gap::IngressWindow::PERIOD {
            let w = super::rx_gap::IngressWindow::between(&self.ingress_since.1, st, elapsed);
            tracing::info!(
                packets = w.packets,
                video_kbps = w.video_kbps,
                fec_repaired = w.fec_repaired,
                frames_dropped = w.frames_dropped,
                rejected = w.rejected,
                max_gap_ms = self.rx_gap.take_max_silence().as_millis() as u64,
                "wire ingress"
            );
            self.ingress_since = (Instant::now(), *st);
        }
    }
}

/// One report window of AU inter-arrival gaps, for `PUNKTFUNK_PERF`.
#[derive(Default)]
struct PerfWindow {
    arrivals_us: Vec<u32>,
    last_arrival: Option<Instant>,
}

impl PerfWindow {
    fn note_au(&mut self, now: Instant) {
        if let Some(prev) = self.last_arrival.replace(now) {
            // 4096 ≈ 17 s at 240 fps — a stuck window cannot grow it unbounded.
            if self.arrivals_us.len() < 4096 {
                self.arrivals_us
                    .push((now - prev).as_micros().min(u32::MAX as u128) as u32);
            }
        }
    }

    /// Log the window's jitter and start the next. `late` = gaps over 2× the
    /// window median (a frame arrived visibly off-beat).
    fn log_and_clear(&mut self) {
        let arrivals_us = &mut self.arrivals_us;
        if arrivals_us.len() >= 8 {
            arrivals_us.sort_unstable();
            let rank = |q| crate::hud::rank(arrivals_us, q);
            let (p50, p95) = (rank(50), rank(95));
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

impl DataPump {
    /// Poll the session until shutdown or a session error: feed the driver,
    /// send what it asks for, close its windows, and queue frames.
    pub(super) fn run(mut self) {
        pin_thread_user_interactive(); // frame channel → user-interactive video pump
        register_hot_tid(&self.shared.hot_tids); // UDP receive + FEC reassembly

        // All-intra: no reference chain, so the channel drains to newest
        // (`FrameChannel::set_all_intra`) instead of strict FIFO.
        self.shared
            .frames
            .set_all_intra(self.negotiated_codec == crate::quic::CODEC_PYROWAVE);
        let session_start = Instant::now();
        let mut lp = PumpLoop {
            abr: self.driver(session_start),
            session_start,
            jump: JumpToLive::new(),
            standing_lat: StandingLatency::new(),
            rx_gap: super::rx_gap::RxGap::new(Instant::now()),
            ingress_since: (Instant::now(), self.session.stats()),
            last_index: None,
            unconsumed_aus: 0,
            seen_clock_gen: self.clock_gen.load(Ordering::Relaxed),
            seen_mode_gen: self.mode_gen.load(Ordering::Relaxed),
            last_epoch: None,
            perf: std::env::var("PUNKTFUNK_PERF")
                .is_ok_and(|v| v != "0")
                .then(PerfWindow::default),
        };
        while !self.shared.shutdown.load(Ordering::SeqCst) {
            // Reloaded every iteration so a mid-stream re-sync hits the
            // next frame's latency math.
            let clock_offset_ns = self.shared.clock_offset.load(Ordering::Relaxed);
            let probe_active = self.feed_driver(&mut lp, clock_offset_ns);
            let tick = lp.abr.tick(Instant::now());
            let request_kbps = self.dispatch(&mut lp.abr, tick.actions);
            if let Some(window) = tick.window {
                self.close_window(&mut lp, window, request_kbps);
            }
            let polled = self.session.poll_frame();
            self.ask_for_short_tails();
            match polled {
                Ok(frame) => self.on_frame(&mut lp, frame, clock_offset_ns, probe_active),
                Err(PunktfunkError::NoFrame) => std::thread::sleep(Duration::from_micros(300)),
                Err(_) => break,
            }
        }
        // Wake a consumer blocked in `next_frame` with Closed, not a timeout.
        self.shared.frames.close();
    }

    /// The session's ABR driver, with the three environment overrides read
    /// once. Automatic is a session with no embedder rate and a host that
    /// echoed one.
    fn driver(&self, session_start: Instant) -> crate::abr::Driver {
        // PyroWave pins the rate (hard per-frame CBR), so Automatic never
        // arms: no AIMD. The bring-up ramp still runs for an Automatic
        // session — to size the pin, not to feed a controller.
        let rate_pinned = self.negotiated_codec == crate::quic::CODEC_PYROWAVE;
        // The pin the Welcome resolved, for a PyroWave Automatic session on a
        // host that serves the ramp. A measured wall lowers it once; nothing
        // raises it.
        let pin_kbps = (rate_pinned && self.bitrate_kbps == 0)
            .then_some(self.resolved_bitrate_kbps)
            .filter(|&pin| pin > 0);
        crate::abr::Driver::new(
            DriverConfig {
                start_kbps: if self.bitrate_kbps == 0 && !rate_pinned {
                    self.resolved_bitrate_kbps
                } else {
                    0
                },
                ceiling_cap_kbps: env_u32("PUNKTFUNK_ABR_MAX_MBPS")
                    .map(|m| m.saturating_mul(1_000)),
                stream_cap_kbps: self.stream_cap_kbps,
                refresh_hz: self.refresh_hz,
                codec: self.negotiated_codec,
                bit_depth: self.bit_depth,
                chroma_format: self.chroma_format,
                audio_reserved_kbps: self.audio_reserved_kbps,
                marks_repeats: self.marks_repeats,
                probe: std::env::var("PUNKTFUNK_ABR_PROBE").map_or(true, |v| v != "0"),
                probe_target_kbps: env_u32("PUNKTFUNK_ABR_PROBE_KBPS"),
                ramp: self.serves_ramp,
                probe_only: self.shared.probe_only(),
                reads_delivery: self.reads_delivery,
                pin_kbps,
            },
            session_start,
        )
    }

    /// Everything the driver hears before this iteration's tick, in the
    /// order it hears it. Returns whether a probe burst is in flight.
    fn feed_driver(&mut self, lp: &mut PumpLoop, clock_offset_ns: i64) -> bool {
        // Re-sync invalidates the staleness run under the old offset.
        let clock_gen = self.clock_gen.load(Ordering::Relaxed);
        if clock_gen != lp.seen_clock_gen {
            lp.seen_clock_gen = clock_gen;
            // Every OWD reading shifted with the offset; the old floor is
            // meaningless. A stale offset that WAS the elevation is fixed here.
            lp.standing_lat.rebase();
            if lp.jump.rebase() {
                tracing::info!("clock re-sync applied — clock-based jump-to-live re-armed");
            }
        }
        // Drain here, not at the report tick, so the in-flight window
        // (the one the rebuild corrupted) is the one we can still drop.
        if let Some(gap_ms) = take_pipeline_gap(&self.pipeline_gap) {
            lp.abr.on_pipeline_gap(gap_ms);
        }
        // Mirror drop/FEC counters every iteration, not only on a
        // produced frame — a total-loss drought completes no AU.
        let st = self.session.stats();
        lp.abr.on_stats(&st);
        // One delay sample per frame that opened since the last iteration,
        // whether or not it ever completed. Same offset and same sign test
        // as a completed AU's; without an offset there is no delay to read,
        // but the samples are still drained.
        for raw_ns in self.session.take_shard_delays() {
            let owd_ns = i128::from(raw_ns) + i128::from(clock_offset_ns);
            if clock_offset_ns != 0 && owd_ns > 0 {
                lp.abr.on_shard_owd(owd_ns);
            }
        }
        lp.watch_ingress(&st);
        self.shared
            .frames_dropped
            .store(st.frames_dropped, Ordering::Relaxed);
        self.shared
            .fec_recovered
            .store(st.fec_recovered_shards, Ordering::Relaxed);
        let (probe_active, probe_duration_ms, probe_report) = self.mirror_probe(&st);
        lp.abr
            .on_probe_active(probe_active, probe_duration_ms, Instant::now());
        if let Some(r) = probe_report {
            lp.abr.on_probe_result(r, Instant::now());
        }
        let mg = self.mode_gen.load(Ordering::Relaxed);
        if mg != lp.seen_mode_gen {
            lp.seen_mode_gen = mg;
            let m = *self.shared.mode.lock().unwrap();
            lp.abr.on_mode_switch(m.width, m.height, m.refresh_hz);
        }
        for (acked, why) in self.bitrate_ack.lock().unwrap().drain(..) {
            lp.abr.on_ack(acked, why);
        }
        // Drain even when the controller is off, so the accumulators
        // stay bounded and no count leaks into a later window.
        let dec = std::mem::take(&mut *self.shared.decode_lat.lock().unwrap());
        lp.abr.on_decode_latency(dec.sum_us, dec.count);
        let enc = std::mem::take(&mut *self.encode_lat.lock().unwrap());
        lp.abr.on_encode_latency(enc.sum_us, enc.count);
        lp.abr
            .on_keyframe_asks(self.recovery_kf.swap(0, Ordering::Relaxed));
        probe_active
    }

    /// Copy the session's probe counters into the shared [`ProbeState`].
    /// Returns `(active, duration_ms, report)`; the report stands while the
    /// state says done.
    fn mirror_probe(&mut self, st: &crate::stats::Stats) -> (bool, u32, Option<ProbeReport>) {
        let mut p = self.shared.probe.lock().unwrap();
        if p.active && !p.done {
            // An embedder speed test is armed on its first mirror
            // tick, which can miss packets a fast link returned in
            // the meantime. The pump's own probes arrive armed.
            let arming = p.base_bytes.is_none();
            if arming {
                self.session.reset_probe_arrivals();
            }
            p.rx_packets_now = st.probe_packets_received;
            p.rx_bytes_now = st.probe_bytes_received;
            (p.first_arrival_ns, p.last_arrival_ns) = if arming {
                (0, 0)
            } else {
                (st.probe_first_arrival_ns, st.probe_last_arrival_ns)
            };
            // The snapshot predates the arm's reset; its gaps belong to the burst before.
            if !arming {
                p.gap_buckets = st.probe_gap_buckets;
                p.reorders = st.probe_reorders;
            }
            p.base_packets.get_or_insert(st.probe_packets_received);
            p.base_bytes.get_or_insert(st.probe_bytes_received);
        } else if p.done && p.ramp {
            // The host's report rides the control stream and the
            // filler rides the data plane, so it can arrive while the
            // bottleneck queue is still handing us the step. Keep
            // counting: the ramp reads the drain, not the send window.
            p.refresh_delivered(st);
        }
        let report = p.done.then(|| ProbeReport {
            delivered_bytes: p.delivered_bytes,
            delivered_packets: p.delivered_packets,
            window_ms: p.throughput_window_ms(p.delivered_packets),
            host_duration_ms: p.host_duration_ms,
            client_interval_ms: p.client_interval_ms,
            client_interval_us: p.client_interval_us,
            host_bytes_sent: p.host_goodput_bytes,
            wire_packets_sent: p.host_wire_packets,
            send_dropped: p.host_send_dropped,
        });
        (p.active && !p.done, p.duration_ms, report)
    }

    /// Send what the tick asked for. Returns the rate this window asked
    /// for, recorded beside the window it came out of.
    fn dispatch(&mut self, abr: &mut crate::abr::Driver, actions: Vec<Action>) -> Option<u32> {
        let mut request_kbps = None;
        for action in actions {
            match action {
                Action::Loss(loss_ppm) => {
                    let _ = self
                        .ctrl_tx
                        .try_send(CtrlRequest::Loss(LossReport { loss_ppm }));
                }
                Action::Delivery(packets_received) => {
                    let _ = self
                        .ctrl_tx
                        .try_send(CtrlRequest::Delivery(DeliveryReport { packets_received }));
                }
                Action::LinkRate(kbps) => {
                    let _ = self.ctrl_tx.try_send(CtrlRequest::LinkRate(kbps));
                }
                Action::SetBitrate(kbps) => {
                    request_kbps = Some(kbps);
                    if self
                        .ctrl_tx
                        .try_send(CtrlRequest::SetBitrate(kbps))
                        .is_err()
                    {
                        // Never reached the control task. Three of these
                        // retire the controller as "the host never acked".
                        abr.on_request_dropped(kbps);
                    }
                }
                Action::Keyframe => {
                    let _ = self.ctrl_tx.try_send(CtrlRequest::Keyframe);
                }
                Action::Probe {
                    target_kbps,
                    duration_ms,
                    ramp,
                } => {
                    // One ProbeState and no correlation id: an embedder
                    // speed test in flight keeps it, and ours is dropped.
                    let mut p = self.shared.probe.lock().unwrap();
                    if p.active && !p.done {
                        drop(p);
                        abr.on_probe_dropped();
                        continue;
                    }
                    // Armed before the request leaves: a fast link hands
                    // back the first packets before the next mirror tick.
                    self.session.reset_probe_arrivals();
                    let armed = self.session.stats();
                    *p = ProbeState {
                        active: true,
                        duration_ms,
                        ramp,
                        base_packets: Some(armed.probe_packets_received),
                        base_bytes: Some(armed.probe_bytes_received),
                        ..Default::default()
                    };
                    drop(p);
                    if self
                        .ctrl_tx
                        .try_send(CtrlRequest::Probe(ProbeRequest {
                            target_kbps,
                            duration_ms,
                        }))
                        .is_err()
                    {
                        self.shared.probe.lock().unwrap().active = false; // ctrl queue full — skip
                        abr.on_probe_dropped();
                    }
                }
                Action::AbandonProbe => self.shared.probe.lock().unwrap().active = false,
            }
        }
        request_kbps
    }

    /// The report window closed: publish it for the embedder, then run the
    /// standing-latency ladder and the per-window logs.
    fn close_window(
        &mut self,
        lp: &mut PumpLoop,
        window: crate::abr::ClosedWindow,
        request_kbps: Option<u32>,
    ) {
        let abr = &lp.abr;
        // Published at the first window, not the moment the ramp
        // stopped: the rate it opened at is the one the host acked,
        // and that ack is still in flight while the ramp finishes.
        if let Some(outcome) = abr.ramp_outcome() {
            let mut slot = self
                .shared
                .abr_ramp
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some(crate::abr::RampRecord {
                    steps: abr.ramp_steps().to_vec(),
                    outcome,
                    opening_kbps: abr.target_kbps(),
                });
            }
        }
        {
            let mut q = self
                .shared
                .abr_windows
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if q.len() == ABR_TRAJECTORY_WINDOWS {
                q.pop_front();
            }
            q.push_back(crate::abr::WindowRecord {
                t_ms: window
                    .sample
                    .now
                    .duration_since(lp.session_start)
                    .as_millis() as u64,
                // The rate the window ran at: the ask has not been
                // acked yet, so it is not this window's rate.
                rate_kbps: abr.target_kbps(),
                request_kbps,
                sample: window.sample,
                discarded: window.discarded,
                reason: abr.reason(),
            });
        }
        // No-op clock flush suspected a wall-clock step: re-sync
        // once. The 60 s periodic covers everything else.
        if lp.jump.take_resync() {
            let _ = self.ctrl_tx.try_send(CtrlRequest::ClockResync);
        }
        // Standing-latency window close. Escalation: re-sync (stale
        // offset), then bleed (flush+keyframe), then disarm (path
        // latency changed).
        match lp.standing_lat.on_window(window.loss_free) {
            StandingLatAction::None => {}
            StandingLatAction::Resync { above_ms } => {
                tracing::info!(
                    above_ms,
                    "standing latency above the session floor with zero loss — \
                     requesting a clock re-sync first (a stale offset reads exactly \
                     like this)"
                );
                let _ = self.ctrl_tx.try_send(CtrlRequest::ClockResync);
            }
            // Shares the jump-to-live cooldown. An unexecuted bleed re-arms
            // as the detector's run rebuilds.
            StandingLatAction::Bleed { above_ms } => {
                if lp.jump.claim_flush(Instant::now()) {
                    // Not a jump-to-live: that is ABR SEVERE (immediate
                    // ×0.7). Bleed fires after ~6 clean windows with a
                    // sub-25 ms elevation the controller already scores as fine.
                    let (flushed, dropped) = self.shed_backlog();
                    lp.standing_lat.bled();
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
        self.shared.rate_cut.store(
            lp.abr
                .last_cut()
                .and_then(crate::hud::RateCut::of_reason)
                .map_or(0, |c| c as u8),
            Ordering::Relaxed,
        );
        if let Some(perf) = lp.perf.as_mut() {
            if let Some(p) = self.session.take_pump_perf() {
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
            perf.log_and_clear();
        }
    }

    /// Ask for recovery the moment a frame's last shard lands short of what its parity can
    /// rebuild, a frame interval before the next frame shows the gap, so the host's next
    /// encode is the anchor. The decode side's gap still arms the freeze. All-intra frames
    /// reference nothing, and a stream nobody decodes yet starts on an IDR: neither asks.
    fn ask_for_short_tails(&mut self) {
        let tails: Vec<u32> = self.session.take_short_tails().collect();
        if tails.is_empty()
            || self.negotiated_codec == crate::quic::CODEC_PYROWAVE
            || !self.shared.frames.consumer_seen()
        {
            return;
        }
        let now = Instant::now();
        for idx in tails {
            if let Some((missing, recovery)) = self.session.missing_beyond_parity(idx) {
                self.shared
                    .short_frames
                    .lock()
                    .unwrap()
                    .note(idx, missing, recovery);
            }
            let ask = self.shared.rfi.lock().unwrap().tail_short(idx, now);
            super::super::send_recovery(&self.shared, &self.ctrl_tx, ask);
        }
    }

    /// One polled frame. Probe filler is skipped, a frame with no decoder
    /// yet is held or dropped, a stale backlog jumps to live, the rest queue.
    fn on_frame(
        &mut self,
        lp: &mut PumpLoop,
        frame: Frame,
        clock_offset_ns: i64,
        probe_active: bool,
    ) {
        if frame.flags & FLAG_PROBE as u32 != 0 {
            return; // speed-test filler, not video — measured via the counters above
        }
        if lp.last_epoch != Some(frame.epoch) {
            lp.last_epoch = Some(frame.epoch);
            let delivered = self.shared.anchor.lock().unwrap().frame(frame.epoch);
            if let Some(mode) = delivered {
                super::anchor::apply(&self.shared.mode, &self.mode_gen, mode);
            }
        }
        // The decoder's RFI for this gap reads what the skipped frame lacks now.
        if let Some(first) =
            super::super::recovery::first_skipped(&mut lp.last_index, frame.frame_index)
        {
            if let Some((missing, recovery)) = self.session.missing_beyond_parity(first) {
                self.shared
                    .short_frames
                    .lock()
                    .unwrap()
                    .note(first, missing, recovery);
            }
        }
        // Prefix parts are not AU arrivals. Inter-arrival, OWD, and the clock
        // detector are per-AU; parts would bias OWD low and reset the staleness run.
        let is_au = frame.complete;
        if is_au {
            // Repeats are the host's idle keepalive, not new content.
            lp.abr
                .on_au(frame.flags & crate::packet::USER_FLAG_REPEAT != 0);
            if let Some(perf) = lp.perf.as_mut() {
                perf.note_au(Instant::now());
            }
        }
        // No decoder yet (a console launch hold delays it up to 15 s). Hold the
        // opening GOP so a prompt decoder starts on the stream's own IDR; past
        // PREROLL_AUS drop it and ask for one keyframe once something pops. A
        // queue nobody drains is not link distress, so no detector runs.
        if !self.shared.frames.consumer_seen() {
            lp.unconsumed_aus += if lp.unconsumed_aus == 0 {
                self.shared.frames.preroll(frame) as u64
            } else {
                u64::from(is_au)
            };
            lp.jump.idle();
            return;
        }
        if lp.unconsumed_aus > 0 {
            tracing::info!(
                dropped_aus = lp.unconsumed_aus,
                "decoder attached after the stream started — asking for a keyframe"
            );
            lp.unconsumed_aus = 0;
            let _ = self.ctrl_tx.try_send(CtrlRequest::Keyframe);
        }
        if probe_active {
            // Probe measures a saturated queue; a primed run would fire the
            // moment the burst ended.
            lp.jump.idle();
        } else if self.jump_to_live(lp, &frame, is_au, clock_offset_ns) {
            return; // this frame is the stale past
        }
        self.shared.frames.push(frame);
    }

    /// Feed one frame's delay to ABR and the standing-latency floor, then to
    /// [`JumpToLive`]. `true` = the backlog was shed, this frame with it.
    fn jump_to_live(
        &mut self,
        lp: &mut PumpLoop,
        frame: &Frame,
        is_au: bool,
        clock_offset_ns: i64,
    ) -> bool {
        let lat_ns = if clock_offset_ns != 0 && is_au {
            now_realtime_ns() + clock_offset_ns as i128 - frame.pts_ns as i128
        } else {
            0
        };
        // Mean capture→received delay. Rising delay under
        // zero loss is queue growth — the pre-loss signal.
        if clock_offset_ns != 0 && lat_ns > 0 {
            lp.abr.on_owd(lat_ns);
            // Window MINIMUM, not mean: a standing state
            // elevates the floor. 10 s clamp matches hn stats.
            if lat_ns < 10_000_000_000 {
                lp.standing_lat.note_frame(lat_ns);
            }
        }
        let depth = self.shared.frames.depth();
        let Some(trip) = lp.jump.observe(Instant::now(), lat_ns, is_au, depth) else {
            return false;
        };
        lp.abr.on_flush(); // SEVERE: the link cannot hold the rate
        let (flushed, dropped) = self.shed_backlog();
        tracing::warn!(
            behind_ms = if trip.clock { lat_ns / 1_000_000 } else { -1 },
            queue_depth = depth,
            flushed_datagrams = flushed,
            dropped_frames = dropped,
            "receive backlog stopped draining — jumped to live (flush + keyframe)"
        );
        match lp.jump.after_flush(trip, flushed, dropped) {
            Shed::Noop { disarmed: false } => {}
            Shed::Noop { disarmed: true } => tracing::warn!(
                "clock-based jump-to-live disarmed — its flushes found no \
                 local backlog (clock step or upstream queueing suspected); \
                 the queue-depth detector stays armed"
            ),
            Shed::Real { sheds } => {
                if self.bitrate_kbps != 0 && sheds == PIN_SHEDS_TO_WARN {
                    self.shared
                        .unsustainable_pin_kbps
                        .store(self.bitrate_kbps, Ordering::Relaxed);
                    tracing::warn!(
                        pinned_kbps = self.bitrate_kbps,
                        sheds,
                        "pinned bitrate above what this client sustains — the \
                         receive backlog keeps being shed"
                    );
                }
            }
        }
        true
    }

    /// Drop the socket backlog and every queued AU, then ask for the
    /// keyframe the next frame decodes from. Returns `(datagrams, AUs)`.
    fn shed_backlog(&mut self) -> (u64, usize) {
        let flushed = self.session.flush_backlog().unwrap_or(0);
        let dropped = self.shared.frames.clear();
        let _ = self.ctrl_tx.try_send(CtrlRequest::Keyframe);
        (flushed, dropped)
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

    /// The loopback sessions' config, for either end.
    fn loopback_config(role: crate::config::Role) -> crate::config::Config {
        crate::config::Config {
            role,
            fec: crate::config::FecConfig {
                scheme: crate::config::FecScheme::Gf16,
                fec_percent: 25,
                max_data_per_block: 32,
            },
            shard_payload: 1024,
            max_frame_bytes: 1 << 20,
            loopback_drop_period: 0,
        }
    }

    /// Unsealed media whose capture times count from `origin`.
    fn loopback_media(origin: u64) -> crate::session::MediaV2 {
        crate::session::MediaV2 {
            clock_origin_ns: origin,
            keys: None,
            clock: None,
        }
    }

    /// Idle client-role loopback, capture times from `origin`. The pump under test is its
    /// report tick, not frames.
    fn idle_client_session(origin: u64) -> (crate::transport::LoopbackTransport, Session) {
        let (host_tp, client_tp) = crate::transport::loopback_pair(0, 0);
        let cfg = loopback_config(crate::config::Role::Client);
        let session = Session::new(cfg, loopback_media(origin), Box::new(client_tp)).unwrap();
        // Keep the host end so the link stays whole for the pump's run.
        (host_tp, session)
    }

    /// A pump on an idle loopback with an explicit rate, so no controller or probe runs.
    fn test_pump(
        session: Session,
        shared: Arc<ClientShared>,
        codec: u8,
    ) -> (DataPump, tokio::sync::mpsc::Receiver<CtrlRequest>) {
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::channel::<CtrlRequest>(8);
        let pump = DataPump {
            session,
            shared,
            ctrl_tx,
            clock_gen: Arc::new(AtomicU32::new(0)),
            encode_lat: Arc::new(Mutex::new(Default::default())),
            mode_gen: Arc::new(AtomicU32::new(0)),
            bitrate_ack: Arc::new(Mutex::new(AckQueue::new())),
            recovery_kf: Arc::new(AtomicU32::new(0)),
            pipeline_gap: Arc::new(AtomicU32::new(0)),
            bitrate_kbps: 20_000,
            resolved_bitrate_kbps: 20_000,
            negotiated_codec: codec,
            bit_depth: 8,
            chroma_format: 0,
            marks_repeats: false,
            serves_ramp: false,
            reads_delivery: false,
            audio_reserved_kbps: 256,
            stream_cap_kbps: 100_000,
            refresh_hz: 60,
        };
        (pump, ctrl_rx)
    }

    /// The first RFI the pump sends within `wait`, skipping its reports.
    fn first_rfi(
        rx: &mut tokio::sync::mpsc::Receiver<CtrlRequest>,
        wait: Duration,
    ) -> Option<(u32, u32)> {
        let deadline = Instant::now() + wait;
        loop {
            match rx.try_recv() {
                Ok(CtrlRequest::Rfi(r)) => return Some((r.first_frame, r.last_frame)),
                Ok(_) => {}
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
                Err(_) => return None,
            }
        }
    }

    /// A frame whose head is lost asks for recovery when its own tail lands, before any
    /// later frame could show the gap. No decode loop runs here, so only the pump can ask.
    /// A PyroWave stream references nothing and asks nothing.
    #[test]
    fn a_frame_whose_head_is_lost_asks_for_recovery_at_its_tail() {
        use crate::transport::Transport;
        let mode = crate::config::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        for (codec, expect) in [
            (crate::quic::CODEC_HEVC, Some((1, 1))),
            (crate::quic::CODEC_PYROWAVE, None),
        ] {
            let origin = crate::quic::wall_clock_ns();
            let (host_tp, session) = idle_client_session(origin);
            let shared = Arc::new(ClientShared::new(mode));
            let _ = shared.frames.pop(Duration::ZERO); // a decoder is attached
            let (pump, mut ctrl_rx) = test_pump(session, shared.clone(), codec);
            let pump_thread = std::thread::spawn(move || pump.run());

            // The host's packets, sealed by a host session and sent here by hand.
            let (spare, _) = crate::transport::loopback_pair(0, 0);
            let mut host = Session::new(
                loopback_config(crate::config::Role::Host),
                loopback_media(origin),
                Box::new(spare),
            )
            .unwrap();
            // 8 data shards, 2 parity: three lost at the head cannot be rebuilt.
            let frame = vec![7u8; 8 * 1024];
            let pts = crate::quic::wall_clock_ns();
            for p in host.seal_frame(&frame, pts, 0).unwrap() {
                host_tp.send(&p).unwrap();
            }
            let lossy = host.seal_frame(&frame, pts + 10_000_000, 0).unwrap();
            assert_eq!(lossy.len(), 10);
            for p in &lossy[3..] {
                host_tp.send(p).unwrap();
            }
            assert_eq!(
                first_rfi(&mut ctrl_rx, Duration::from_millis(500)),
                expect,
                "codec {codec}"
            );

            shared.shutdown.store(true, Ordering::SeqCst);
            pump_thread.join().unwrap();
        }
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
        use crate::quic::v2::io as v2io;
        v2io::send(&mut host_send, &crate::quic::RequestKeyframe)
            .await
            .expect("open the stream with a message the client ignores");
        let (ctrl_send, ctrl_recv) = accept_ctrl.await.unwrap();

        let pipeline_gap = Arc::new(AtomicU32::new(0));
        // Hold the sender so the task does not exit on a closed channel.
        let (_task_ctrl_tx, task_ctrl_rx) = tokio::sync::mpsc::channel::<CtrlRequest>(8);
        let (clip_event_tx, _clip_event_rx) = std::sync::mpsc::sync_channel(8);
        let (cursor_shape_tx, _cursor_shape_rx) = crate::client::planes::shape_queue();
        let (access_tx, _access_rx) = std::sync::mpsc::sync_channel(8);
        let (hidout_tx, _hidout_rx) = std::sync::mpsc::sync_channel(8);
        let mode = crate::config::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        tokio::spawn(
            super::super::control_task::ControlTask {
                ctrl_rx: task_ctrl_rx,
                ctrl_send,
                ctrl_recv: CtlRecv::new(ctrl_recv),
                clock_rtt_ns: None, // no connect handshake ⇒ no re-sync batches to interleave
                shared: Arc::new(ClientShared::new(mode)),
                bitrate_ack: Arc::new(Mutex::new(AckQueue::new())),
                recovery_kf: Arc::new(AtomicU32::new(0)),
                pipeline_gap: pipeline_gap.clone(),
                clock_gen: Arc::new(AtomicU32::new(0)),
                clip_event_tx,
                cursor_shape_tx,
                mode_gen: Arc::new(AtomicU32::new(0)),
                access_tx,
                hidout_tx,
            }
            .run(),
        );

        // Explicit bitrate (not Automatic): keep the controller and the
        // startup probe out. The probe would discard a window of its own.
        let pump_shared = Arc::new(ClientShared::new(mode));
        let (_host_tp, session) = idle_client_session(crate::quic::wall_clock_ns());
        let (mut pump, mut pump_ctrl_rx) =
            test_pump(session, pump_shared.clone(), crate::quic::CODEC_HEVC);
        pump.pipeline_gap = pipeline_gap.clone();
        let started = Instant::now();
        let pump_thread = std::thread::spawn(move || pump.run());

        // Mid-window, as a rebuild actually lands: 200 ms into 750 ms.
        tokio::time::sleep(Duration::from_millis(200)).await;
        v2io::send(&mut host_send, &crate::quic::PipelineGap { gap_ms: 401 })
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

        pump_shared
            .shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        pump_thread.join().unwrap();
    }
}
