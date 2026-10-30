//! Blocking data-plane pump: poll the session, run Adaptive-FEC / ABR /
//! jump-to-live / standing-latency, and hand frames to the embedder.
//!
//! Dedicated user-interactive thread. Newest-frame drop on embedder lag.
//! [`FLAG_PROBE`] filler never enters the decoder. The ABR window is
//! assembled by [`crate::abr::feed::Feed`]: [`abr_feed`] reads what it hears
//! and sends what it asks for. [`recovery_asks`] asks for what a short frame lost.

use super::super::*;
use super::*;

mod abr_feed;
mod recovery_asks;

/// Data-plane pump on a blocking thread. `try_send` drops the newest frame
/// when the embedder lags. [`FLAG_PROBE`] filler goes to the probe accumulator,
/// not the decoder.
pub(super) struct DataPump {
    pub(super) session: Session,
    pub(super) shared: Arc<ClientShared>,
    pub(super) ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    /// The control task's [`PumpEvent`]s, drained once per iteration ([`drain_events`]).
    pub(super) events: std::sync::mpsc::Receiver<PumpEvent>,
    /// The pump's own end of that channel: a frame that opens a configured epoch moves the
    /// mode through it, as the control task does.
    pub(super) events_tx: PumpTx,
    /// Host encode-stage window ([`super::super::frame_channel::EncodeLatAcc`]);
    /// fed by the datagram task, not the overlay's lossy `host_timing_tx`.
    pub(super) encode_lat: Arc<Mutex<super::super::frame_channel::EncodeLatAcc>>,
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
    /// Audio-plane wire reservation, spent whether video flows or not.
    pub(super) audio_reserved_kbps: u32,
    /// Mode+codec ceiling ([`crate::abr::stream_ceiling_kbps`]) for the
    /// negotiated geometry, PyroWave's pin; recomputed by the driver on a mode switch.
    pub(super) stream_cap_kbps: u32,
    /// PyroWave: [`crate::pyrowave::BPP_FLOOR`] at the negotiated geometry.
    pub(super) pyrowave_floor_kbps: Option<u32>,
    /// Negotiated refresh, not the request still sitting in `shared.mode`.
    pub(super) refresh_hz: u32,
    /// A short frame may ask for its missing shards: whole AUs only, so not under
    /// slice-progressive delivery.
    pub(super) nack: bool,
    /// The newest whole frame was an anchor: the host references only frames this client
    /// confirmed, so the next frame skips a lost one and its tail asks nothing.
    pub(super) on_anchors: bool,
    /// All-intra frames short past their parity and asking nothing: each goes up partial
    /// once its head block is whole.
    pub(super) partial_wait: Vec<u32>,
    /// Frames of an epoch whose config has not arrived, oldest first ([`Self::hand_on`]).
    pub(super) held: std::collections::VecDeque<Frame>,
    /// When the oldest of them started waiting.
    pub(super) held_since: Option<Instant>,
}

/// How long a frame waits for its epoch's config. The host sends the config before the
/// epoch's first packet, so a wait is the two paths' skew, well under this.
const HOLD_FOR: Duration = Duration::from_millis(250);
/// Frames that wait at most. 16 is 67 ms at 240 Hz, longer than the paths' skew.
const HOLD_MAX: usize = 16;

/// What one pump run carries from iteration to iteration.
struct PumpLoop {
    /// Each frame goes to its driver here; the rest in [`DataPump::feed_driver`].
    feed: crate::abr::feed::Feed,
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
    /// That keyframe has not arrived: the decoder holds none of the pictures the frames
    /// before it reference, and a hardware decoder fed one can die.
    awaiting_idr: bool,
    /// Epoch of the last frame handed on; a new one may move the mode ([`super::anchor`]).
    last_epoch: Option<u8>,
    /// When the socket's drop count was last read.
    sock_read: Instant,
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
        let mut lp = PumpLoop {
            feed: self.feed(Instant::now()),
            jump: JumpToLive::new(),
            standing_lat: StandingLatency::new(),
            rx_gap: super::rx_gap::RxGap::new(Instant::now()),
            ingress_since: (Instant::now(), self.session.stats()),
            last_index: None,
            unconsumed_aus: 0,
            awaiting_idr: false,
            last_epoch: None,
            sock_read: Instant::now(),
            perf: std::env::var("PUNKTFUNK_PERF")
                .is_ok_and(|v| v != "0")
                .then(PerfWindow::default),
        };
        while !self.shared.shutdown.load(Ordering::SeqCst) {
            // Reloaded every iteration so a mid-stream re-sync hits the
            // next frame's latency math.
            let clock_offset_ns = self.shared.clock_offset.load(Ordering::Relaxed);
            let (probe_active, tick) = self.feed_driver(&mut lp, clock_offset_ns);
            let request_kbps = self.dispatch(&mut lp.feed.driver, tick.actions);
            if let Some(window) = tick.window {
                self.close_window(&mut lp, window, request_kbps);
            }
            if let Some(memory) = self.shared.au_memory.lock().unwrap().take() {
                self.session.set_au_memory(memory);
            }
            self.session.set_partial_hold(self.shared.frames.held());
            let polled = self.session.poll_frame();
            self.ask_for_short_tails();
            let now = Instant::now();
            let resend = self.shared.feedback.lock().unwrap().resend(now);
            if let Some(fb) = resend {
                self.shared.send_feedback(&fb);
            }
            // The driver's clock: the drop climb, the overdue freeze and a deferred RFI.
            let frames_dropped = self.shared.frames_dropped.load(Ordering::Relaxed);
            self.recovery(super::super::RecoveryEvent::Tick { frames_dropped }, now);
            self.release_waiting_partials();
            self.release_held(now);
            match self.shared.frames.overdue_hold(now) {
                Some(frame)
                    if self.negotiated_codec == crate::quic::CODEC_PYROWAVE
                        && self.session.release_partial(frame, false) =>
                {
                    tracing::debug!(
                        "nack expired: the frame goes up partial, then the ones after it"
                    );
                }
                Some(_) if self.shared.frames.expire_hold(now) => {
                    tracing::debug!("nack expired: the frames after it go, and their gap asks");
                }
                _ => {}
            }
            for _ in 0..self.shared.frames.take_filled() {
                tracing::debug!("nack filled: the held frames go in order");
                self.session.note_nack(true);
            }
            for ask in self.shared.screens.tick(now) {
                self.shared.send_feedback(&ask);
            }
            match polled {
                Ok(frame) => self.on_frame(&mut lp, frame, clock_offset_ns, probe_active),
                // Idle wire: a datagram wakes this at once; 1 ms bounds the timers above.
                Err(PunktfunkError::NoFrame) => {
                    if self.session.wait_packets(Duration::from_millis(1)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // Wake a consumer blocked in `next_frame` with Closed, not a timeout.
        self.shared.frames.close();
        self.shared.screens.close();
    }

    /// One polled frame. Probe filler is skipped, a frame with no decoder
    /// yet is held or dropped, a stale backlog jumps to live, the rest queue.
    /// The keyframe a late decoder asks for is the client's own, never recovery's.
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
        if frame.stream != crate::packet::V2_STREAM_VIDEO {
            if let Some(ask) = self.shared.screens.on_frame(frame, Instant::now()) {
                self.shared.send_feedback(&ask);
            }
            return;
        }
        self.shared
            .feedback
            .lock()
            .unwrap()
            .on_frame(frame.frame_index, frame.flags);
        if lp.last_epoch != Some(frame.epoch) {
            lp.last_epoch = Some(frame.epoch);
            let delivered = self
                .shared
                .anchor
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .frame(frame.epoch);
            if let Some(mode) = delivered {
                super::anchor::apply(&self.shared.mode, &self.events_tx, mode);
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
            self.on_anchors = frame.flags & crate::packet::USER_FLAG_RECOVERY_ANCHOR != 0;
            // Repeats are the host's idle keepalive, not new content.
            lp.feed
                .driver
                .on_au(frame.flags & crate::packet::USER_FLAG_REPEAT != 0);
            if let Some(perf) = lp.perf.as_mut() {
                perf.note_au(Instant::now());
            }
        }
        // No decoder yet (a console launch hold delays it up to 15 s). Hold the
        // opening GOP so a prompt decoder starts on the stream's own IDR; past
        // PREROLL_AUS drop it, ask for one keyframe once something pops and hand
        // on nothing before it. A queue nobody drains is not link distress, so
        // no detector runs.
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
            lp.awaiting_idr = self.negotiated_codec != crate::quic::CODEC_PYROWAVE;
            self.shared.ask_starved_keyframe();
        }
        if lp.awaiting_idr {
            if frame.flags & u32::from(crate::packet::FLAG_SOF) == 0 {
                lp.jump.idle();
                return;
            }
            lp.awaiting_idr = false;
        }
        if probe_active {
            // Probe measures a saturated queue; a primed run would fire the
            // moment the burst ended.
            lp.jump.idle();
        } else if self.jump_to_live(lp, &frame, is_au, clock_offset_ns) {
            return; // this frame is the stale past
        }
        self.hand_on(frame);
    }

    /// Queue `frame` for the decoder. Under format epochs a frame whose epoch has no config
    /// yet waits, and the frames after it with it, so a decoder is never fed a codec or depth
    /// it was not built for. Everything before this point already counted the frame.
    fn hand_on(&mut self, frame: Frame) {
        let ready = self.held.is_empty()
            && self
                .shared
                .anchor
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .ready(frame.epoch);
        if ready {
            self.shared.frames.push(frame);
            return;
        }
        self.held_since.get_or_insert_with(Instant::now);
        self.held.push_back(frame);
        self.release_held(Instant::now());
    }

    /// Hand on each waiting frame whose epoch's config is in. Past [`HOLD_FOR`] or
    /// [`HOLD_MAX`] the rest go too, decoding as the newest configured epoch: a lost config
    /// costs a decode error and a keyframe, not a frozen stream.
    fn release_held(&mut self, now: Instant) {
        let Some(since) = self.held_since else {
            return;
        };
        let expired = now.duration_since(since) >= HOLD_FOR || self.held.len() > HOLD_MAX;
        let mut ready = 0;
        {
            let mut anchor = self.shared.anchor.lock().unwrap_or_else(|e| e.into_inner());
            for f in &self.held {
                if !anchor.ready(f.epoch) {
                    if !expired {
                        break;
                    }
                    tracing::warn!(
                        epoch = f.epoch,
                        waited_ms = now.duration_since(since).as_millis() as u64,
                        frames = self.held.len(),
                        "no config arrived for this epoch — its frames go on in the last format"
                    );
                    anchor.assume(f.epoch);
                }
                ready += 1;
            }
        }
        for f in self.held.drain(..ready) {
            self.shared.frames.push(f);
        }
        self.held_since = match (self.held.is_empty(), ready) {
            (true, _) => None,
            (false, 0) => Some(since),
            (false, _) => Some(now),
        };
    }

    /// Feed one frame's capture → received delay to ABR, which nets out the
    /// host's share by pts, and unchanged to the standing-latency floor and
    /// [`JumpToLive`]. ABR hears of a jump only once its flush is judged
    /// ([`Shed::tell`]). `true` = the backlog was shed, this frame with it.
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
            lp.feed.driver.on_owd(frame.pts_ns, lat_ns);
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
        let (flushed, dropped) = self.shed_backlog();
        tracing::warn!(
            behind_ms = if trip.clock { lat_ns / 1_000_000 } else { -1 },
            decoder = trip.decoder,
            queue_depth = depth,
            flushed_datagrams = flushed,
            dropped_frames = dropped,
            "receive backlog stopped draining — jumped to live (flush + keyframe)"
        );
        let shed = lp.jump.after_flush(trip, flushed, dropped);
        shed.tell(trip, &mut lp.feed.driver);
        match shed {
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
    /// keyframe the next frame decodes from. Returns `(datagrams, AUs)`. The
    /// ask is the client's own, never recovery's: the client threw the frames
    /// away, and a late link already reads as the flush.
    fn shed_backlog(&mut self) -> (u64, usize) {
        let flushed = self.session.flush_backlog().unwrap_or(0);
        let dropped = self.shared.frames.clear() + self.held.len();
        self.held.clear();
        self.held_since = None;
        self.shared.ask_starved_keyframe();
        (flushed, dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            clear_placement: false,
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

    type FeedbackRx = std::sync::mpsc::Receiver<crate::quic::v2::dgram::Feedback>;

    /// A pump on an idle loopback with an explicit rate, so no controller or probe runs. Its
    /// feedback datagrams land on the returned receiver.
    fn test_pump(
        session: Session,
        shared: Arc<ClientShared>,
        codec: u8,
    ) -> (
        DataPump,
        tokio::sync::mpsc::Receiver<CtrlRequest>,
        FeedbackRx,
    ) {
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::channel::<CtrlRequest>(8);
        let (fb_tx, fb_rx) = std::sync::mpsc::channel();
        let _ = shared.feedback_tx.set(Box::new(move |fb| {
            let _ = fb_tx.send(*fb);
        }));
        let (events_tx, events) = std::sync::mpsc::channel();
        let pump = DataPump {
            session,
            shared,
            ctrl_tx,
            events,
            events_tx,
            encode_lat: Arc::new(Mutex::new(Default::default())),
            bitrate_kbps: 20_000,
            resolved_bitrate_kbps: 20_000,
            negotiated_codec: codec,
            bit_depth: 8,
            chroma_format: 0,
            marks_repeats: false,
            serves_ramp: false,
            audio_reserved_kbps: 256,
            stream_cap_kbps: 100_000,
            pyrowave_floor_kbps: None,
            refresh_hz: 60,
            nack: false,
            on_anchors: false,
            partial_wait: Vec::new(),
            held: std::collections::VecDeque::new(),
            held_since: None,
        };
        (pump, ctrl_rx, fb_rx)
    }

    fn frame_of(epoch: u8, index: u32) -> Frame {
        Frame {
            data: Vec::new().into(),
            frame_index: index,
            pts_ns: 0,
            flags: 0,
            epoch,
            stream: 0,
            complete: true,
            part: None,
            received_ns: 0,
            new_format: None,
        }
    }

    /// Under format epochs a frame of an epoch with no config waits, the frames behind it with
    /// it, and all go in order once the config is in. Past the bound they go in the last format.
    #[test]
    fn a_frame_waits_for_its_epochs_config_then_goes_in_order() {
        use crate::quic::{VideoFormat, CODEC_H264, CODEC_HEVC};
        let (_host_tp, session) = idle_client_session(0);
        let shared = Arc::new(ClientShared::new(crate::config::Mode::default()));
        {
            let mut a = shared.anchor.lock().unwrap();
            (a.on, a.formats) = (true, true);
            a.book(
                0,
                VideoFormat {
                    codec: CODEC_HEVC,
                    ..VideoFormat::default()
                },
            );
        }
        let (mut pump, _ctrl, _fb) = test_pump(session, shared.clone(), CODEC_HEVC);
        pump.hand_on(frame_of(0, 1));
        pump.hand_on(frame_of(1, 2));
        pump.hand_on(frame_of(1, 3));
        assert_eq!(shared.frames.depth(), 1, "epoch 1 has no config yet");

        let cfg = crate::quic::v2::msg::StreamConfig {
            epoch: 1,
            codec: CODEC_H264,
            ..Default::default()
        };
        shared.anchor.lock().unwrap().config(cfg);
        pump.release_held(Instant::now());
        assert_eq!(shared.frames.depth(), 3);

        pump.hand_on(frame_of(2, 4));
        assert_eq!(shared.frames.depth(), 3);
        pump.release_held(Instant::now() + HOLD_FOR);
        assert_eq!(shared.frames.depth(), 4, "the bound lets it go");
        let assumed = shared.anchor.lock().unwrap().format(2).map(|f| f.codec);
        assert_eq!(assumed, Some(CODEC_H264), "in the newest configured format");
    }

    /// Without format epochs nothing waits.
    #[test]
    fn without_format_epochs_no_frame_waits() {
        let (_host_tp, session) = idle_client_session(0);
        let shared = Arc::new(ClientShared::new(crate::config::Mode::default()));
        shared.anchor.lock().unwrap().on = true;
        let (mut pump, _ctrl, _fb) = test_pump(session, shared.clone(), crate::quic::CODEC_HEVC);
        pump.hand_on(frame_of(5, 1));
        assert_eq!(shared.frames.depth(), 1);
        assert!(pump.held.is_empty());
    }

    /// The first RFI the pump sends within `wait`, skipping its reports.
    fn first_rfi(rx: &FeedbackRx, wait: Duration) -> Option<(u32, u32)> {
        let deadline = Instant::now() + wait;
        loop {
            match rx.try_recv() {
                Ok(fb) if fb.ask != 0 && fb.invalidate.is_some() => return fb.invalidate,
                Ok(_) => {}
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
                Err(_) => return None,
            }
        }
    }

    /// A frame whose head is lost asks for recovery when its own tail lands, before any
    /// later frame could show the gap. No decode loop runs here, so only the pump can ask.
    /// A PyroWave stream references nothing and asks nothing, nor does a frame after an
    /// anchor: the host references only confirmed frames, so the next one skips it.
    #[test]
    fn a_frame_whose_head_is_lost_asks_for_recovery_at_its_tail() {
        use crate::transport::Transport;
        let mode = crate::config::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let anchor = crate::packet::USER_FLAG_RECOVERY_ANCHOR;
        for (codec, flags, expect) in [
            (crate::quic::CODEC_HEVC, 0, Some((1, 1))),
            (crate::quic::CODEC_PYROWAVE, 0, None),
            (crate::quic::CODEC_HEVC, anchor, None),
        ] {
            let origin = crate::quic::wall_clock_ns();
            let (host_tp, session) = idle_client_session(origin);
            let shared = Arc::new(ClientShared::new(mode));
            let _ = shared.frames.pop(Duration::ZERO); // a decoder is attached
            let (pump, _ctrl_rx, fb_rx) = test_pump(session, shared.clone(), codec);
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
            for p in host.seal_frame(&frame, pts, flags).unwrap() {
                host_tp.send(&p).unwrap();
            }
            let lossy = host.seal_frame(&frame, pts + 10_000_000, flags).unwrap();
            assert_eq!(lossy.len(), 10);
            for p in &lossy[3..] {
                host_tp.send(p).unwrap();
            }
            assert_eq!(
                first_rfi(&fb_rx, Duration::from_millis(500)),
                expect,
                "codec {codec}"
            );

            shared.shutdown.store(true, Ordering::SeqCst);
            pump_thread.join().unwrap();
        }
    }

    /// A decoder that attaches after the preroll is gone starts on the keyframe it asked for:
    /// the frames before it reference pictures it never had.
    #[test]
    fn a_late_decoder_starts_on_the_keyframe_it_asked_for() {
        use crate::packet::{FLAG_PIC, FLAG_SOF};
        use crate::transport::Transport;
        let mode = crate::config::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let origin = crate::quic::wall_clock_ns();
        let (host_tp, session) = idle_client_session(origin);
        let shared = Arc::new(ClientShared::new(mode));
        let (pump, _ctrl_rx, _fb_rx) = test_pump(session, shared.clone(), crate::quic::CODEC_HEVC);
        let pump_thread = std::thread::spawn(move || pump.run());
        let (spare, _) = crate::transport::loopback_pair(0, 0);
        let mut host = Session::new(
            loopback_config(crate::config::Role::Host),
            loopback_media(origin),
            Box::new(spare),
        )
        .unwrap();
        let mut pts = crate::quic::wall_clock_ns();
        let mut send = |flags: u8| {
            pts += 16_666_667;
            for p in host.seal_frame(&[9u8; 512], pts, u32::from(flags)).unwrap() {
                host_tp.send(&p).unwrap();
            }
        };
        let (idr, p) = (FLAG_PIC | FLAG_SOF, FLAG_PIC);
        send(idr);
        for _ in 0..super::super::frame_channel::PREROLL_AUS + 4 {
            send(p);
        }
        std::thread::sleep(Duration::from_millis(200));
        // The decoder attaches with the opening GOP already dropped.
        assert!(matches!(
            shared.frames.pop(Duration::ZERO),
            FramePop::Timeout
        ));
        for _ in 0..3 {
            send(p);
        }
        send(idr);
        send(p);
        let first = match shared.frames.pop(Duration::from_secs(1)) {
            FramePop::Frame(f) => f,
            _ => panic!("no frame after the keyframe"),
        };
        assert_ne!(
            first.flags & u32::from(FLAG_SOF),
            0,
            "the decoder's first frame is the IDR"
        );
        assert!(matches!(
            shared.frames.pop(Duration::from_secs(1)),
            FramePop::Frame(f) if f.flags & u32::from(FLAG_SOF) == 0
        ));

        shared.shutdown.store(true, Ordering::SeqCst);
        pump_thread.join().unwrap();
    }

    /// A PyroWave frame short past its parity, with no NACK in reach, goes up partial ahead of
    /// the next frame and is not a dropped one: every tenth frame loses a tail shard on a round
    /// trip past the frame interval, and every frame reaches the decoder, in order.
    #[test]
    fn a_pyrowave_frame_without_its_tail_goes_up_partial() {
        use crate::packet::USER_FLAG_CHUNK_ALIGNED;
        use crate::transport::Transport;
        let mode = crate::config::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let origin = crate::quic::wall_clock_ns();
        let (host_tp, mut session) = idle_client_session(origin);
        session.set_deliver_partial_frames(true);
        let shared = Arc::new(ClientShared::new(mode));
        let _ = shared.frames.pop(Duration::ZERO); // a decoder is attached
        shared.rtt_us.store(40_000, Ordering::Relaxed);
        let (mut pump, _ctrl_rx, _fb_rx) =
            test_pump(session, shared.clone(), crate::quic::CODEC_PYROWAVE);
        pump.nack = true;
        let pump_thread = std::thread::spawn(move || pump.run());

        let (spare, _) = crate::transport::loopback_pair(0, 0);
        let mut host = Session::new(
            loopback_config(crate::config::Role::Host),
            loopback_media(origin),
            Box::new(spare),
        )
        .unwrap();
        // An 11-shard head with four parity, then 40 tail shards in blocks of 32 and 8.
        let au = crate::pyrowave::windowed_au(1024, 11, 40);
        let mut got = Vec::new();
        for i in 0..40u32 {
            let wires = host
                .seal_frame(&au, crate::quic::wall_clock_ns(), USER_FLAG_CHUNK_ALIGNED)
                .unwrap();
            for (k, w) in wires.iter().enumerate() {
                if !(i % 10 == 9 && k == 20) {
                    host_tp.send(w).unwrap();
                }
            }
            let deadline = Instant::now() + Duration::from_millis(500);
            while got.last().map(|&(i, _)| i) != Some(i) && Instant::now() < deadline {
                if let FramePop::Frame(f) = shared.frames.pop(Duration::from_millis(1)) {
                    got.push((f.frame_index, f.complete));
                }
            }
            assert_eq!(
                got.last().map(|&(i, _)| i),
                Some(i),
                "frame {i} before the next"
            );
        }
        shared.shutdown.store(true, Ordering::SeqCst);
        pump_thread.join().unwrap();
        let want: Vec<(u32, bool)> = (0..40).map(|i| (i, i % 10 != 9)).collect();
        assert_eq!(got, want, "every frame, the short ones partial, in order");
        assert_eq!(shared.frames_dropped.load(Ordering::Relaxed), 0);
    }

    /// Frames short of their parity on a round trip inside a frame interval ask for their
    /// missing shards, and the host's resend completes them in order: every 50th frame loses
    /// sixteen data shards, eight past its parity; every frame arrives, none is dropped, no
    /// RFI goes out. Nine past is congestion: an RFI, as without NACK. A PyroWave frame asks
    /// for its shards the same way. A frame that completed before its tail was read asks
    /// nothing.
    #[test]
    fn short_frames_ask_for_their_shards_and_complete_in_order() {
        use crate::quic::{CODEC_HEVC, CODEC_PYROWAVE};
        use crate::transport::Transport;
        let mode = crate::config::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        for (codec, lost, expect_rfi) in [
            (CODEC_HEVC, 16usize, false),
            (CODEC_HEVC, 17, true),
            (CODEC_PYROWAVE, 16, false),
        ] {
            let origin = crate::quic::wall_clock_ns();
            let (host_tp, session) = idle_client_session(origin);
            let shared = Arc::new(ClientShared::new(mode));
            let _ = shared.frames.pop(Duration::ZERO); // a decoder is attached
            shared.rtt_us.store(10_000, Ordering::Relaxed);
            let (mut pump, _ctrl_rx, fb_rx) = test_pump(session, shared.clone(), codec);
            pump.nack = true;
            let pump_thread = std::thread::spawn(move || pump.run());

            let (spare, _) = crate::transport::loopback_pair(0, 0);
            let mut host = Session::new(
                loopback_config(crate::config::Role::Host),
                loopback_media(origin),
                Box::new(spare),
            )
            .unwrap();
            host.tap_plaintext(true);
            // 32 KiB in 1 KiB shards: one block of 32 data and 8 parity, in wire order.
            let frame = vec![7u8; 32 * 1024];
            let (mut nacks, mut rfis, mut got) = (0, 0, Vec::new());
            for i in 0..100u32 {
                let wires = host
                    .seal_frame(&frame, crate::quic::wall_clock_ns(), 0)
                    .unwrap();
                let mut plain = Vec::new();
                host.drain_plaintext(|p| plain.push(p.to_vec()));
                let short = i % 50 == 49;
                for (k, w) in wires.iter().enumerate() {
                    if !(short && k < lost) {
                        host_tp.send(w).unwrap();
                    }
                }
                let deadline = Instant::now() + Duration::from_millis(500);
                while got.last() != Some(&i) && Instant::now() < deadline {
                    while let Ok(fb) = fb_rx.try_recv() {
                        if let Some(n) = fb.nack.filter(|n| n.frame == i) {
                            nacks += 1;
                            for &s in n.shards() {
                                host_tp
                                    .send(&host.reseal(&plain[usize::from(s)]).unwrap())
                                    .unwrap();
                            }
                        }
                        rfis += usize::from(fb.ask != 0 && fb.invalidate.is_some());
                    }
                    if let FramePop::Frame(f) = shared.frames.pop(Duration::from_millis(1)) {
                        got.push(f.frame_index);
                    }
                    if short && expect_rfi && rfis > 0 {
                        break;
                    }
                }
            }
            shared.shutdown.store(true, Ordering::SeqCst);
            pump_thread.join().unwrap();
            if expect_rfi {
                assert!(
                    rfis > 0 && nacks == 0,
                    "{lost} lost: {rfis} RFIs, {nacks} NACKs"
                );
            } else {
                let all = (0..100).collect::<Vec<u32>>();
                assert_eq!(got, all, "codec {codec}: every frame, in order");
                assert_eq!((nacks > 0, rfis), (true, 0), "codec {codec}");
                assert_eq!(shared.frames_dropped.load(Ordering::Relaxed), 0);
            }
        }
    }

    /// Every report lost, and every other datagram after it: the acks a 60 Hz decoder sends
    /// still bring each window to the host, gap-free and in order.
    #[test]
    fn a_lost_report_rides_the_next_ack() {
        let mode = crate::config::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let (_host_tp, session) = idle_client_session(crate::quic::wall_clock_ns());
        let shared = Arc::new(ClientShared::new(mode));
        // The lossy link goes in first; `test_pump`'s own sender is then refused.
        let (tx, rx) = std::sync::mpsc::channel();
        let link = Mutex::new((0u32, 0u32));
        let _ = shared.feedback_tx.set(Box::new(move |fb| {
            let (last, n) = &mut *link.lock().unwrap();
            let lost = if fb.window != *last {
                *last = fb.window;
                true
            } else {
                *n += 1;
                *n % 2 == 1
            };
            if !lost {
                let _ = tx.send(fb.window);
            }
        }));
        let (pump, _ctrl_rx, _) = test_pump(session, shared.clone(), crate::quic::CODEC_HEVC);
        let pump_thread = std::thread::spawn(move || pump.run());
        let (mut seen, mut index) = (Vec::new(), 0u32);
        let deadline = Instant::now() + Duration::from_secs(4);
        while seen.last() != Some(&3) && Instant::now() < deadline {
            let idr = if index == 0 {
                crate::packet::FLAG_SOF
            } else {
                0
            };
            shared.decoder_took(index, idr.into(), true);
            index += 1;
            std::thread::sleep(Duration::from_millis(16));
            seen.extend(rx.try_iter().filter(|&w| w != 0));
            seen.dedup();
        }
        shared.shutdown.store(true, Ordering::SeqCst);
        pump_thread.join().unwrap();
        assert_eq!(seen, [1, 2, 3]);
    }

    /// A full demux queue's drops are the receiver's own: the window reports them as
    /// `sock_drops`, with or without a kernel figure beside them.
    #[test]
    fn demux_queue_drops_report_as_the_receivers_own() {
        let mode = crate::config::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let (_host_tp, session) = idle_client_session(crate::quic::wall_clock_ns());
        let shared = Arc::new(ClientShared::new(mode));
        let demux = Arc::new(crate::transport::shared::SharedStats::default());
        demux.media_dropped.store(40, Ordering::Relaxed);
        *shared.demux.lock().unwrap() = Some(demux);
        let (pump, _ctrl_rx, fb_rx) = test_pump(session, shared.clone(), crate::quic::CODEC_HEVC);
        let pump_thread = std::thread::spawn(move || pump.run());
        let deadline = Instant::now() + Duration::from_secs(2);
        let report = loop {
            match fb_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(fb) if fb.window != 0 => break Some(fb),
                Ok(_) => {}
                Err(_) => break None,
            }
        };
        shared.shutdown.store(true, Ordering::SeqCst);
        pump_thread.join().unwrap();
        assert_eq!(report.map(|fb| fb.sock_drops), Some(40));
    }

    /// Host-rebuild repair, end to end: a real [`PipelineGap`] on a real
    /// control stream, the control task sends it on, the pump discards the
    /// window it landed in.
    ///
    /// Assertions watch the window's report — the discarded window's
    /// only externally visible product on an idle session. A near-zero
    /// denominator would have the host raise FEC against a link that
    /// never dropped. The next window must report: discard is one wide.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn a_host_pipeline_gap_discards_the_report_window_in_flight() {
        let (_server_ep, _client_ep, host_conn, client_conn) =
            crate::quic::test_util::connect_pair().await;
        // Host opens the control stream (normally the client does during
        // handshake): this host end only writes, so a client-opened
        // stream would stay invisible.
        let accept_ctrl = tokio::spawn(async move { client_conn.accept_bi().await.unwrap() });
        let (mut host_send, _host_recv) = host_conn.open_bi().await.unwrap();
        use crate::quic::v2::io as v2io;
        v2io::send(&mut host_send, &crate::quic::SetBitrate { bitrate_kbps: 1 })
            .await
            .expect("open the stream with a message the client ignores");
        let (ctrl_send, ctrl_recv) = accept_ctrl.await.unwrap();

        let mode = crate::config::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        // Explicit bitrate (not Automatic): keep the controller and the
        // startup probe out. The probe would discard a window of its own.
        let pump_shared = Arc::new(ClientShared::new(mode));
        let (_host_tp, session) = idle_client_session(crate::quic::wall_clock_ns());
        let (pump, _pump_ctrl_rx, fb_rx) =
            test_pump(session, pump_shared.clone(), crate::quic::CODEC_HEVC);

        // Hold the sender so the task does not exit on a closed channel.
        let (_task_ctrl_tx, task_ctrl_rx) = tokio::sync::mpsc::channel::<CtrlRequest>(8);
        let (planes, _planes_rx) = crate::client::planes::channels();
        tokio::spawn(
            super::super::control_task::ControlTask {
                ctrl_rx: task_ctrl_rx,
                ctrl_send,
                ctrl_recv: CtlRecv::new(ctrl_recv),
                clock_rtt_ns: None, // no connect handshake ⇒ no re-sync batches to interleave
                shared: Arc::new(ClientShared::new(mode)),
                events: pump.events_tx.clone(),
                clip_event_tx: planes.clip_event,
                cursor_shape_tx: planes.cursor_shape,
                access_tx: planes.access,
                hidout_tx: planes.datagram.hidout,
            }
            .run(),
        );
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
            fb_rx.try_iter().all(|fb| fb.window == 0),
            "the window the host's rebuild landed in must be discarded, not reported"
        );

        // Next window must report: the gap discards one window, not a second. A wedged
        // pump would fail here rather than pass the discard assert above.
        let reported = tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + Duration::from_millis(1_500);
            loop {
                match fb_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(fb) if fb.window != 0 => return Some(fb),
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        })
        .await
        .unwrap();
        assert!(
            reported.is_some(),
            "the window after the gap must produce its report on schedule"
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
