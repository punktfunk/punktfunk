//! The send thread: sealed access units out of the encode loop and onto the wire.
//!
//! [`send_loop`] owns the socket side — chunking under `send_pacing`, the reconfig gate, and
//! the per-second [`SendStats`] line. The encode loop hands it [`FrameMsg`]s and never blocks
//! on a write.

use super::*;

/// The AU-level fields of one encoded AU: the same on its whole-AU message and on every
/// chunk of a streamed one.
#[derive(Clone, Copy)]
pub(super) struct AuMeta {
    pub(super) capture_ns: u64,
    pub(super) flags: u32,
    /// [`super::state::StreamState::epoch`] when this AU was encoded.
    pub(super) epoch: u8,
    /// Predicted at submit as `au_seq + inflight`; stamped on the wire so RFI stays 1:1 across rebuilds.
    pub(super) frame_index: u32,
    /// Next frame's due time. Past = send immediately (catch up).
    pub(super) deadline: std::time::Instant,
    pub(super) encode_us: u32,
    /// Delivery→submit age (µs). 0 for repeats/tail. Wire pts anchors at the same delivery stamp.
    pub(super) queue_us: u32,
    /// `cap_us` = `try_latest`; `submit_us` = encode launch; `wait_us` = lock_bitstream.
    /// Synchronous backends (PyroWave) put the whole encode in `submit_us` — `wait_us` reads ~0.
    pub(super) cap_us: u32,
    pub(super) submit_us: u32,
    pub(super) wait_us: u32,
    pub(super) repeat: bool,
    /// Trust this, not a re-read of `is_armed()`: a capture that arms mid-flight must not fold
    /// zeroed splits into the first window's percentiles.
    pub(super) was_measured: bool,
    /// The Windows driver encoded this AU; `queue_us`/`encode_us` are then its own stages.
    pub(super) driver: Option<crate::stats_recorder::DriverSample>,
}

/// One encoded AU handed to the send thread. Encode of N+1 overlaps transmit of N.
pub(super) struct FrameMsg {
    pub(super) data: Vec<u8>,
    pub(super) meta: AuMeta,
}

/// Whole AU, or one slice-boundary chunk of a streamed AU (seal/pace while the encoder still runs).
pub(super) enum SendMsg {
    Frame(FrameMsg),
    Chunk(ChunkMsg),
}

/// One encoder chunk of a streamed AU. Splits matter on `last`.
pub(super) struct ChunkMsg {
    pub(super) data: Vec<u8>,
    pub(super) first: bool,
    pub(super) last: bool,
    pub(super) meta: AuMeta,
}

/// Open streamed AU: incremental sealer plus pace aggregation across per-chunk flushes.
struct StreamedOpen {
    au: punktfunk_core::packet::StreamedAu,
    spread_us: u32,
    paced: bool,
    /// One microburst budget per AU, consumed across flushes. Per-flush auto granted each block
    /// its own 128 KiB. `None` = pacing off (`PUNKTFUNK_PACE_FACTOR=0`, no burst pin).
    burst_left: Option<usize>,
}

/// Open at `first`, seal+pace completed FEC blocks, close at `last`. `None` mid-AU.
fn handle_chunk(
    session: &mut Session,
    open: &mut Option<StreamedOpen>,
    c: ChunkMsg,
    slice_wire: bool,
    pacing: &mut crate::send_pacing::Pacing,
) -> Result<Option<(AuMeta, PaceStat)>> {
    let m = c.meta;
    if c.first {
        if open.take().is_some() {
            // Rebuild forfeits the in-flight AU; sentinel packets are already on the wire.
            tracing::warn!(
                "streamed AU abandoned mid-flight (encoder rebuild) — client ages it out"
            );
        }
        // USER_FLAG_SLICE_STREAM only toward a client that negotiated streamed AUs AND multi-slice.
        let flags = m.flags
            | if slice_wire {
                punktfunk_core::packet::USER_FLAG_SLICE_STREAM
            } else {
                0
            }
            | if m.repeat {
                punktfunk_core::packet::USER_FLAG_REPEAT
            } else {
                0
            };
        *open = Some(StreamedOpen {
            au: session
                .begin_streamed_frame_at(m.capture_ns, flags, m.frame_index)
                .map_err(|e| anyhow!("begin_streamed_frame: {e:?}"))?,
            spread_us: 0,
            paced: false,
            burst_left: if pacing.pace_rate_bps == 0 && pacing.burst_cap.is_none() {
                None
            } else {
                Some(pacing.burst_bytes(0))
            },
        });
    }
    let Some(s) = open.as_mut() else {
        return Err(anyhow!(
            "streamed chunk without an open AU (encode-loop bug)"
        ));
    };
    // Chunked poll returns per-slice; the AU's flag gates whether the sealer cuts a block there.
    let wires = session
        .seal_streamed_chunk(&mut s.au, &c.data, true)
        .map_err(|e| anyhow!("seal_streamed_chunk: {e:?}"))?;
    if !wires.is_empty() {
        // Charge the flush's full wire size. Over-count paces later blocks sooner (the safe direction).
        let flush_bytes: usize = wires.iter().map(|w| w.len()).sum();
        let stat = pace_sealed(
            session,
            wires,
            m.deadline,
            s.burst_left.or(pacing.burst_cap),
            pacing,
        )?;
        if let Some(left) = s.burst_left.as_mut() {
            *left = left.saturating_sub(flush_bytes);
        }
        s.spread_us = s.spread_us.saturating_add(stat.spread_us);
        s.paced |= stat.paced;
    }
    if !c.last {
        return Ok(None);
    }
    let s = open.take().expect("checked above");
    let tail = session
        .seal_streamed_finish(s.au)
        .map_err(|e| anyhow!("seal_streamed_finish: {e:?}"))?;
    let stat = pace_sealed(
        session,
        tail,
        m.deadline,
        s.burst_left.or(pacing.burst_cap),
        pacing,
    )?;
    Ok(Some((
        m,
        PaceStat {
            spread_us: s.spread_us.saturating_add(stat.spread_us),
            paced: s.paced || stat.paced,
        },
    )))
}

/// One 2 s window of per-AU send timings, read by the perf line and the stats recorder.
#[derive(Default)]
struct PerfWindow {
    encode_us: Vec<u32>,
    pace_us: Vec<u32>,
    /// Capture → fully sent; probes excluded.
    host_us: Vec<u32>,
    cap_us: Vec<u32>,
    submit_us: Vec<u32>,
    wait_us: Vec<u32>,
    queue_us: Vec<u32>,
    driver: crate::stats_recorder::DriverStages,
    paced: u64,
    immediate: u64,
    new: u64,
    repeats: u64,
}

impl PerfWindow {
    fn record(&mut self, m: &AuMeta, stat: &PaceStat, host_us: Option<u32>) {
        self.encode_us.push(m.encode_us);
        self.pace_us.push(stat.spread_us);
        self.host_us.extend(host_us);
        if m.was_measured {
            match m.driver {
                Some(d) => self.driver.note(d),
                None => {
                    self.cap_us.push(m.cap_us);
                    self.submit_us.push(m.submit_us);
                    if !m.repeat {
                        self.queue_us.push(m.queue_us);
                    }
                }
            }
            self.wait_us.push(m.wait_us);
        }
        if m.repeat {
            self.repeats += 1;
        } else {
            self.new += 1;
        }
        if stat.paced {
            self.paced += 1;
        } else {
            self.immediate += 1;
        }
    }

    /// The recorder's stages: the driver path's own, then the copy and the send; else the
    /// host's.
    fn stages(&mut self) -> Vec<crate::stats_recorder::StageTiming> {
        use crate::stats_recorder::stage;
        match self.driver.stages() {
            Some(mut s) => {
                s.extend([
                    stage("copy", &mut self.wait_us),
                    stage("send", &mut self.pace_us),
                ]);
                s
            }
            None => vec![
                stage("queue", &mut self.queue_us),
                stage("capture", &mut self.cap_us),
                stage("submit", &mut self.submit_us),
                stage("encode", &mut self.wait_us),
                stage("send", &mut self.pace_us),
            ],
        }
    }

    /// Capture → sent `(p50, p99)`; `None` when no AU went out.
    fn host(&mut self) -> Option<(f32, f32)> {
        (!self.host_us.is_empty()).then(|| {
            (
                percentile(&mut self.host_us, 0.50) as f32,
                percentile(&mut self.host_us, 0.99) as f32,
            )
        })
    }
}

/// 0xCF, stamped against the same capture anchor the wire pts carries. On the driver, queue
/// is its pool wait and encode its encode; the client's residual is hand-off, copy and seal.
fn send_host_timing(
    tc: &crate::native::link::SessionLink,
    m: &AuMeta,
    stat: &PaceStat,
    host_us: u32,
) {
    let t = punktfunk_core::quic::HostTiming {
        pts_ns: m.capture_ns,
        host_us,
        stages: Some(punktfunk_core::quic::HostStages {
            queue_us: m.queue_us,
            encode_us: m.encode_us,
            pace_us: stat.spread_us,
        }),
    };
    let _ = tc.send_datagram(punktfunk_core::quic::encode_host_timing_datagram(&t));
}

/// Inputs the send thread needs for the 2 s web-console sample.
pub(super) struct SendStats {
    pub(super) rec: Arc<StatsRecorder>,
    /// Packed w:16|h:16|hz:16. Capture thread updates it on a mid-stream mode switch.
    pub(super) mode: Arc<AtomicU64>,
    pub(super) codec: &'static str,
    pub(super) client: String,
    pub(super) plane: crate::events::Plane,
    pub(super) bitrate_kbps: Arc<AtomicU32>,
    /// What the client's ramp proved the link carries (kbps); `0` = no report yet.
    pub(super) link_kbps: Arc<AtomicU32>,
    /// A pinned stream (PyroWave) is paced against `link_kbps`: its rate says
    /// nothing about the link. An adaptive stream keeps the factor.
    pub(super) link_paced: bool,
    /// The profile this session streams under (`DeliveryProfile as u8`): what the
    /// client asked for. `PUNKTFUNK_DELIVERY` overrides it.
    pub(super) delivery: Arc<std::sync::atomic::AtomicU8>,
    pub(super) bringup: Arc<crate::bringup::Trace>,
    /// Data-socket clone for the kernel-queue probe behind the `wire egress` line.
    pub(super) wire_sock: Option<std::net::UdpSocket>,
    /// Frames the Windows driver dropped at its pool, session-cumulative. Written by the
    /// encode thread from the driver's telemetry; stays 0 off the driver.
    pub(super) driver_dropped: Arc<AtomicU64>,
    /// Sealed wire bytes go here each aggregation tick; the control task diffs them into the
    /// per-minute `link health` line's `egress_mbps`.
    pub(super) counters: Arc<crate::session_status::SessionCounters>,
}

/// Pace rate for one frame, bits/s: the stream rate times the factor. A stream paced
/// against the link takes the rate the client's ramp proved instead, whether that is
/// above the factor or below it, once the stream fits in 70 % of it — the share of a
/// measured wall the ramp licenses. Past the proof a frame queues at the slowest port
/// on the path: a 1 MB PyroWave frame at 3× its rate overflows a 2.5 GbE client port
/// behind a 10 GbE host. `factor` 0 keeps the deadline-only spread whatever the link.
fn pace_rate_bps(bitrate_kbps: u32, factor: f64, link_kbps: Option<u32>) -> u64 {
    if factor == 0.0 {
        return 0;
    }
    let stream = u64::from(bitrate_kbps) * 1000;
    let by_link = link_kbps.map_or(0, |k| u64::from(k) * 1000);
    if by_link > 0 && by_link * 7 >= stream * 10 {
        return by_link;
    }
    (stream as f64 * factor) as u64
}

/// Whether this session may accept a mid-stream `Reconfigure`.
///
/// Off for gamescope (a resize respawns the nested game), a per-client-mode identity (the mode
/// is part of the slot key, so a resize is a different display), and a `shared` display: a
/// monitor mirror (`design/per-monitor-portal-capture.md`) or a `mode_conflict: join` session.
/// Both stream a display whose mode belongs to someone else. The client scales.
pub(crate) fn reconfig_allowed(
    compositor: Option<crate::vdisplay::Compositor>,
    per_client_mode: bool,
    shared: bool,
) -> bool {
    compositor != Some(crate::vdisplay::Compositor::Gamescope) && !per_client_mode && !shared
}

#[allow(clippy::too_many_arguments)]
pub(super) fn send_loop(
    session: Session,
    frame_rx: std::sync::mpsc::Receiver<SendMsg>,
    probe_rx: std::sync::mpsc::Receiver<ProbeShaped>,
    probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    stop: Arc<AtomicBool>,
    perf: bool,
    // Smoothed whole-AU paced-send µs. The split arbiter prices HEVC overlap from this; only this
    // thread sees a send.
    send_spread_us: Arc<AtomicU32>,
    wire_rekeys: Arc<AtomicU32>,
    slice_wire: bool,
    burst_cap: Option<usize>,
    fec_target: Arc<AtomicU8>,
    // Applied between AUs only — a streamed AU's tiling is derived from the size it began with.
    shard_rx: std::sync::mpsc::Receiver<usize>,
    mut stats: SendStats,
    timing_conn: Option<crate::native::link::SessionLink>,
    probe_seq: bool,
) {
    boost_thread_priority(false);
    let wire = WireLine::new(stats.wire_sock.take());
    let mut lp = SendLoop {
        session,
        probe_rx,
        probe_result_tx,
        perf,
        send_spread_us,
        wire_rekeys,
        slice_wire,
        fec_target,
        shard_rx,
        timing_conn,
        probe_seq,
        // 3× default: the link carries 1× sustained, so a bounded 3× excursion is safe (WebRTC
        // uses 2.5×). `PUNKTFUNK_PACE_FACTOR=0` restores deadline-only spread.
        pace_factor: std::env::var("PUNKTFUNK_PACE_FACTOR")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|f: &f64| f.is_finite() && *f >= 0.0)
            .unwrap_or(3.0),
        forced: crate::send_pacing::forced_delivery(),
        pacing: crate::send_pacing::Pacing::new(burst_cap),
        link_gso: false,
        streamed: None,
        burst: None,
        perf_line: PerfLine::new(&stats),
        wire,
        stats,
    };
    // Idle tick: with no AU in hand the loop still revisits `stop`, the FEC target and the
    // 2 s stats window.
    const IDLE_TICK: std::time::Duration = std::time::Duration::from_millis(50);
    apply_fec_target(&mut lp.session, &lp.fec_target);
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        lp.service_probe();
        // Wake when the burst's next filler is due, so its rate holds while video shares the
        // loop. Mid-AU it cannot be pumped, so the idle tick stands.
        let wait = match lp.burst.as_ref() {
            Some(b) if lp.streamed.is_none() => b.next_due().min(IDLE_TICK),
            _ => IDLE_TICK,
        };
        match frame_rx.recv_timeout(wait) {
            Ok(send_msg) => {
                if !lp.step(send_msg) {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        lp.housekeeping();
    }
    // Stop, teardown, or a dead channel mid-burst: report what went out, leave nothing armed.
    if let Some(b) = lp.burst {
        let _ = lp.probe_result_tx.send(b.finish());
    }
}

/// The send thread's state across AUs: the session, the pacing inputs, the open streamed AU,
/// the live probe burst and the two periodic log lines.
struct SendLoop {
    session: Session,
    probe_rx: std::sync::mpsc::Receiver<ProbeShaped>,
    probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    perf: bool,
    send_spread_us: Arc<AtomicU32>,
    wire_rekeys: Arc<AtomicU32>,
    slice_wire: bool,
    fec_target: Arc<AtomicU8>,
    shard_rx: std::sync::mpsc::Receiver<usize>,
    stats: SendStats,
    timing_conn: Option<crate::native::link::SessionLink>,
    probe_seq: bool,
    pace_factor: f64,
    forced: Option<crate::send_pacing::DeliveryProfile>,
    pacing: crate::send_pacing::Pacing,
    link_gso: bool,
    streamed: Option<StreamedOpen>,
    burst: Option<ProbeBurst>,
    perf_line: PerfLine,
    wire: WireLine,
}

impl SendLoop {
    /// Between AUs: the probe burst's next filler, or a new burst the client asked for.
    fn service_probe(&mut self) {
        // Never mid-AU: a burst spliced between streamed chunks would push the tail past its deadline.
        if self.streamed.is_none() {
            service_burst(
                &mut self.session,
                &mut self.burst,
                &self.probe_rx,
                &self.probe_result_tx,
                self.probe_seq,
            );
        }
    }

    /// After every turn: egress for the governor, the FEC target and a pending shard re-key
    /// for the next AU, then the two periodic lines.
    fn housekeeping(&mut self) {
        // The share window closes on a client delivery report, one per 750 ms
        // report window. A counter published on a 2 s clock gives that window
        // nothing twice and then 2 s of bytes, which reads as a path refusing
        // everything it was offered ([`crate::session_status::share_for`]).
        self.stats
            .counters
            .link
            .publish_egress_bytes(self.session.stats().bytes_sent);
        apply_fec_target(&mut self.session, &self.fec_target);
        if self.streamed.is_none() {
            let mut want_shard = None;
            while let Ok(s) = self.shard_rx.try_recv() {
                want_shard = Some(s);
            }
            if let Some(s) = want_shard {
                match self.session.set_shard_payload(s) {
                    Ok(()) => {
                        self.wire_rekeys.fetch_add(1, Ordering::Relaxed);
                        tracing::info!(shard_payload = s, "wire shard payload re-keyed");
                    }
                    Err(e) => tracing::warn!(shard_payload = s, error = ?e,
                        "shard re-key refused by session validation"),
                }
            }
        }
        self.wire.maybe_log(&self.session);
        self.perf_line.maybe_log(
            &mut self.session,
            &self.stats,
            self.timing_conn.as_ref(),
            self.perf,
        );
    }

    /// One message off the encode loop: pace it out and account for it. `false` when a send
    /// failed and the stream ends.
    fn step(&mut self, send_msg: SendMsg) -> bool {
        let stats = &self.stats;
        let bitrate_kbps = stats.bitrate_kbps.load(Ordering::Relaxed);
        let link_kbps = stats.link_kbps.load(Ordering::Relaxed);
        let pace_rate = pace_rate_bps(
            bitrate_kbps,
            self.pace_factor,
            stats.link_paced.then_some(link_kbps),
        );
        // A link-rate burst is a super-buffer train: GSO cuts its send calls 3×.
        if !self.link_gso && pace_rate > pace_rate_bps(bitrate_kbps, self.pace_factor, None) {
            self.link_gso = true;
            self.session.set_gso(true);
            tracing::info!(link_kbps, "pacing at the client's proven link rate, GSO on");
        }
        // Bound one frame's spread to ~2 intervals so a big IDR cannot back the channel
        // into `cadence_degraded`. hz 0 = not yet known → the absolute ceiling alone.
        let (_, _, hz) = unpack_mode(stats.mode.load(Ordering::Relaxed));
        let max_spread = if hz > 0 {
            std::time::Duration::from_secs_f64(2.0 / hz as f64)
        } else {
            crate::send_pacing::MAX_PACE_SPREAD
        };
        let profile = self.forced.unwrap_or_else(|| {
            crate::send_pacing::DeliveryProfile::from_u8(stats.delivery.load(Ordering::Relaxed))
        });
        self.pacing.update(pace_rate, max_spread, profile);
        // A new epoch takes effect at the AU that carries it, never mid-AU.
        match &send_msg {
            SendMsg::Frame(f) => self.session.set_epoch(f.meta.epoch),
            SendMsg::Chunk(c) if c.first => self.session.set_epoch(c.meta.epoch),
            SendMsg::Chunk(_) => {}
        }
        let outcome = match send_msg {
            SendMsg::Frame(FrameMsg { data, meta: m }) => paced_submit(
                &mut self.session,
                &data,
                m.capture_ns,
                // HOST_CAP2_REPEAT_MARK makes the bit's absence mean "new content".
                m.flags
                    | if m.repeat {
                        punktfunk_core::packet::USER_FLAG_REPEAT
                    } else {
                        0
                    },
                m.frame_index,
                m.deadline,
                &mut self.pacing,
            )
            .map(|stat| Some((m, stat))),
            SendMsg::Chunk(c) => handle_chunk(
                &mut self.session,
                &mut self.streamed,
                c,
                self.slice_wire,
                &mut self.pacing,
            ),
        };
        match outcome {
            Ok(None) => {}
            Ok(Some((m, stat))) => self.sent(&m, &stat),
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "send failed — stopping stream");
                return false;
            }
        }
        true
    }

    /// An AU is on the wire: the bring-up mark, host timing, the spread EWMA and the windows.
    fn sent(&mut self, m: &AuMeta, stat: &PaceStat) {
        let probe = m.flags & FLAG_PROBE as u32 != 0;
        if !probe {
            self.stats.bringup.finish("first_packet");
        }
        let host_us = (now_ns().saturating_sub(m.capture_ns) / 1000).min(u32::MAX as u64) as u32;
        if let Some(tc) = self.timing_conn.as_ref().filter(|_| !probe) {
            send_host_timing(tc, m, stat, host_us);
        }
        // EWMA (3:1): a single AU's spread must not flip the split-arbiter verdict.
        let prev = self.send_spread_us.load(Ordering::Relaxed);
        let next = if prev == 0 {
            stat.spread_us
        } else {
            ((prev as u64 * 3 + stat.spread_us as u64) / 4) as u32
        };
        self.send_spread_us.store(next, Ordering::Relaxed);
        if self.perf || self.stats.rec.is_armed() {
            self.perf_line
                .win
                .record(m, stat, (!probe).then_some(host_us));
            self.wire.probe.sample();
        }
    }
}

/// The 30 s `wire egress` line: the layer under this thread. Always on: a stall there leaves
/// every other line clean.
struct WireLine {
    probe: crate::net_health::WireProbe,
    last: std::time::Instant,
    sent: u64,
    dropped: u64,
}

impl WireLine {
    fn new(sock: Option<std::net::UdpSocket>) -> Self {
        WireLine {
            probe: crate::net_health::WireProbe::new(sock),
            last: std::time::Instant::now(),
            sent: 0,
            dropped: 0,
        }
    }

    fn maybe_log(&mut self, session: &Session) {
        if self.last.elapsed() < std::time::Duration::from_secs(30) {
            return;
        }
        let s = session.stats();
        let w = self.probe.window();
        tracing::info!(
            sent = s.packets_sent - self.sent,
            send_dropped = s.packets_send_dropped - self.dropped,
            outq_max_kb = w.outq_max_kb,
            tx_dropped = w.tx_dropped,
            tx_errors = w.tx_errors,
            carrier_changes = w.carrier_changes,
            udp_sndbuf_errors = w.udp_sndbuf_errors,
            iface = self.probe.iface.as_deref().unwrap_or("?"),
            "wire egress"
        );
        self.sent = s.packets_sent;
        self.dropped = s.packets_send_dropped;
        self.last = std::time::Instant::now();
    }
}

/// The 2 s window: the `PUNKTFUNK_PERF` line and the stats recorder's sample.
struct PerfLine {
    win: PerfWindow,
    last: std::time::Instant,
    last_bytes: u64,
    last_send_dropped: u64,
    last_driver_dropped: u64,
    sid: Option<(u64, u32)>,
}

impl PerfLine {
    fn new(stats: &SendStats) -> Self {
        PerfLine {
            win: PerfWindow::default(),
            last: std::time::Instant::now(),
            last_bytes: 0,
            last_send_dropped: 0,
            last_driver_dropped: stats.driver_dropped.load(Ordering::Relaxed),
            sid: None,
        }
    }

    fn maybe_log(
        &mut self,
        session: &mut Session,
        stats: &SendStats,
        timing_conn: Option<&crate::native::link::SessionLink>,
        perf: bool,
    ) {
        if self.last.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        let win = &mut self.win;
        let s = session.stats();
        let secs = self.last.elapsed().as_secs_f64();
        let tx_mbps = (s.bytes_sent - self.last_bytes) as f64 * 8.0 / secs / 1_000_000.0;
        // One window of seal timing feeds both the perf line and the recorder. It runs only
        // while one of them reads it.
        let seal_perf = session.take_seal_perf();
        session.set_seal_perf(perf || stats.rec.is_armed());
        if perf {
            let sp = seal_perf.unwrap_or_default();
            tracing::info!(
                tx_mbps = format!("{tx_mbps:.0}"),
                send_dropped = s.packets_send_dropped - self.last_send_dropped,
                send_dropped_total = s.packets_send_dropped,
                encode_us_p50 = percentile(&mut win.encode_us, 0.50),
                encode_us_p99 = percentile(&mut win.encode_us, 0.99),
                pace_us_p50 = percentile(&mut win.pace_us, 0.50),
                pace_us_p99 = percentile(&mut win.pace_us, 0.99),
                pace_us_max = win.pace_us.last().copied().unwrap_or(0),
                immediate_frames = win.immediate,
                paced_frames = win.paced,
                window_ms = format!("{:.0}", secs * 1000.0),
                fec_ms = format!("{:.2}", sp.fec_ns as f64 / 1e6),
                seal_ms = format!("{:.2}", sp.seal_ns as f64 / 1e6),
                sock_ms = format!("{:.2}", sp.sock_ns as f64 / 1e6),
                fec_ns_pp = sp.fec_ns.checked_div(sp.packets).unwrap_or(0),
                seal_ns_pp = sp.seal_ns.checked_div(sp.packets).unwrap_or(0),
                sock_ns_pp = sp.sock_ns.checked_div(sp.packets).unwrap_or(0),
                sealed_pkts = sp.packets,
                "perf"
            );
        }
        let driver_dropped = stats.driver_dropped.load(Ordering::Relaxed);
        if stats.rec.is_armed() {
            let session_id = stats.rec.session_id(&mut self.sid, || {
                let (w, h, hz) = unpack_mode(stats.mode.load(Ordering::Relaxed));
                stats.rec.register_session(
                    stats.plane.as_str(),
                    w,
                    h,
                    hz,
                    stats.codec,
                    &stats.client,
                )
            });
            let stages = win.stages();
            let host = win.host();
            let (fec_us, seal_us, sock_us) = match seal_perf.filter(|p| p.frames > 0) {
                Some(p) => {
                    let per_frame = |ns: u64| Some(ns as f32 / p.frames as f32 / 1000.0);
                    (
                        per_frame(p.fec_ns),
                        per_frame(p.seal_ns),
                        per_frame(p.sock_ns),
                    )
                }
                None => (None, None, None),
            };
            let sample = crate::stats_recorder::StatsSample {
                t_ms: 0,
                session_id,
                stages,
                fec_us,
                seal_us,
                sock_us,
                fps: (win.new as f64 / secs) as f32,
                repeat_fps: (win.repeats as f64 / secs) as f32,
                mbps: tx_mbps as f32,
                bitrate_kbps: stats.bitrate_kbps.load(Ordering::Relaxed),
                frames_dropped: win
                    .driver
                    .active()
                    .then(|| driver_dropped.saturating_sub(self.last_driver_dropped) as u32),
                packets_dropped: None,
                send_dropped: Some(
                    s.packets_send_dropped
                        .saturating_sub(self.last_send_dropped) as u32,
                ),
                fec_recovered: None,
                host_p50_us: host.map(|h| h.0),
                host_p99_us: host.map(|h| h.1),
                rtt_us: timing_conn.map(|c| c.rtt().as_micros().min(u128::from(u32::MAX)) as u32),
            };
            stats.rec.push_sample(session_id, sample);
        }
        self.last_driver_dropped = driver_dropped;
        self.win = PerfWindow::default();
        self.last = std::time::Instant::now();
        self.last_bytes = s.bytes_sent;
        self.last_send_dropped = s.packets_send_dropped;
    }
}

#[cfg(test)]
mod tests {
    use super::{pace_rate_bps, AuMeta, PaceStat, PerfWindow};

    /// A host AU feeds the host stages and a driver AU the driver's. A repeat never counts
    /// toward the queue stage, and a probe never toward capture → sent.
    #[test]
    fn a_perf_window_files_each_au_under_its_own_stages() {
        let meta = AuMeta {
            capture_ns: 0,
            epoch: 0,
            flags: 0,
            frame_index: 0,
            deadline: std::time::Instant::now(),
            encode_us: 900,
            queue_us: 300,
            cap_us: 100,
            submit_us: 200,
            wait_us: 400,
            repeat: false,
            was_measured: true,
            driver: None,
        };
        let stat = PaceStat {
            spread_us: 50,
            paced: true,
        };
        let names = |w: &mut PerfWindow| -> Vec<String> {
            w.stages().into_iter().map(|s| s.name).collect()
        };
        let mut w = PerfWindow::default();
        w.record(&meta, &stat, Some(1_000));
        let repeat = AuMeta {
            repeat: true,
            queue_us: 9_999,
            ..meta
        };
        w.record(
            &repeat,
            &PaceStat {
                paced: false,
                ..stat
            },
            None,
        );
        let unmeasured = AuMeta {
            was_measured: false,
            ..meta
        };
        w.record(&unmeasured, &stat, Some(2_000));
        assert_eq!((w.new, w.repeats, w.paced, w.immediate), (2, 1, 2, 1));
        assert_eq!(
            (w.queue_us.len(), w.cap_us.len(), w.encode_us.len()),
            (1, 2, 3)
        );
        assert_eq!(w.host_us, [1_000, 2_000]);
        assert_eq!(
            names(&mut w),
            ["queue", "capture", "submit", "encode", "send"]
        );

        let mut d = PerfWindow::default();
        let lump = crate::stats_recorder::DriverSample::Lump(Some(700));
        d.record(
            &AuMeta {
                driver: Some(lump),
                ..meta
            },
            &stat,
            None,
        );
        assert!(d.cap_us.is_empty() && d.host().is_none());
        assert_eq!(names(&mut d), ["driver", "copy", "send"]);
    }

    #[test]
    fn a_pinned_stream_paces_at_the_proven_link_rate() {
        assert_eq!(pace_rate_bps(778_000, 3.0, None), 2_334_000_000);
        // Above the factor: a 10 GbE path spends no time it does not need.
        assert_eq!(pace_rate_bps(778_000, 3.0, Some(8_900_000)), 8_900_000_000);
        // Under the factor: a 2.5 GbE client behind a 10 GbE host is sent its own rate.
        assert_eq!(
            pace_rate_bps(1_427_000, 3.0, Some(2_450_000)),
            2_450_000_000
        );
        // A proof the stream does not fit under is no pace: the factor stands.
        assert_eq!(pace_rate_bps(778_000, 3.0, Some(1_000_000)), 2_334_000_000);
        assert_eq!(pace_rate_bps(778_000, 3.0, Some(0)), 2_334_000_000);
        assert_eq!(pace_rate_bps(778_000, 0.0, Some(8_900_000)), 0);
    }
}
