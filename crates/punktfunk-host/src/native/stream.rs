//! Native `punktfunk/1` capture→encode→send data plane.
//!
//! Owns the synthetic and software protocol-test sources, speed-test probes, the paced submit,
//! and [`SessionContext`], the per-session inputs `serve_session` hands to [`virtual_stream`],
//! around the [`StreamCommon`] every source takes.
//! The virtual-display loop itself is [`state::StreamState`]: bring-up in `new`, the tick loop
//! in `run`, one file per concern under `stream/`.
//!
//! Pin `PUNKTFUNK_IDD_ADAPTIVE=0`, `PUNKTFUNK_PACE_FACTOR=0`, `PUNKTFUNK_STREAMED_AU=0` for the
//! rebuild-free A/B levers. Evidence: `design/midstream-resolution-resize.md`.

use super::wiring::{SessionShared, StreamEnds};
use super::*;
use crate::send_pacing::{frame_driven_enabled, CaptureCredit};

mod cursor;
mod encode;
mod pipeline;
mod ramp;
mod rebuild;
mod recovery;
#[cfg(target_os = "windows")]
mod resize;
mod send;
mod session_watch;
mod state;
mod synth_abr;
pub(super) use self::pipeline::{prepare_display, PrepHandle, PreparedDisplay};
// `control.rs` bounds its spacing exemption by the same step length.
pub(crate) use self::ramp::RAMP_STEP_MAX_MS;
use self::send::{send_loop, AuMeta, ChunkMsg, FrameMsg, SendMsg, SendStats};
// `native.rs` asks before offering a mid-stream reconfig.
pub(crate) use self::send::reconfig_allowed;
use self::session_watch::{session_watch_enabled, session_watcher_loop, SessionSwitch};
use self::state::StreamState;
// `main.rs` parses the content script; `native.rs` builds the context and dispatches.
pub(super) use self::synth_abr::{synthetic_abr_stream, SynthAbrContext};
pub use self::synth_abr::{Content, KeyframeAnswer, SynthAbrShape, DEFAULT_IDR_PCT};

/// Test frames at 60 Hz. The frames have no size, so an accepted mode switch is delivered as
/// the next epoch, the way a rebuilt pipeline delivers one.
#[allow(clippy::too_many_arguments)]
pub(super) fn synthetic_stream(
    session: &mut Session,
    frames: u32,
    stop: &AtomicBool,
    ends: &super::wiring::StreamEnds,
    fec_target: &AtomicU8,
    timing_conn: Option<&super::link::SessionLink>,
    probe_seq: bool,
) -> Result<()> {
    let (probe_rx, probe_result_tx) = (&ends.probe_rx, &ends.probe_result_tx);
    let interval = std::time::Duration::from_millis(1000 / 60);
    let mut epoch = 0u8;
    for idx in 0..frames {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if let Some(mode) = ends.reconfig.try_iter().last() {
            epoch = epoch.wrapping_add(1);
            session.set_epoch(epoch);
            let _ = ends.reconfig_result_tx.send(super::wiring::Delivered {
                mode,
                epoch,
                corrects: false,
            });
        }
        apply_fec_target(session, fec_target);
        service_probes(session, stop, probe_rx, probe_result_tx, probe_seq);
        let data = test_frame(idx, 64 * 1024);
        let pts_ns = now_ns();
        session
            .submit_frame(&data, pts_ns, (FLAG_PIC | FLAG_SOF) as u32)
            .map_err(|e| anyhow!("submit_frame: {e:?}"))?;
        // 0xCF host_us is near-zero here (no capture/encode); the datagram still proves the plane.
        if let Some(tc) = timing_conn {
            let t = punktfunk_core::quic::HostTiming {
                pts_ns,
                host_us: (now_ns().saturating_sub(pts_ns) / 1000).min(u32::MAX as u64) as u32,
                stages: None,
            };
            let _ = tc.send_datagram(punktfunk_core::quic::encode_host_timing_datagram(&t));
        }
        std::thread::sleep(interval);
    }
    tracing::info!(frames, "synthetic stream complete");
    Ok(())
}

/// A moving picture through the software H.264 encoder until the session stops. Linux only,
/// because the encoder is; elsewhere a client gets a refusal it can read, not a hang.
///
/// Named, not resolved from the ladder: `auto` never picks software, and `PUNKTFUNK_ENCODER`
/// is latched long before a session arrives.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
pub(super) fn software_stream(
    session: &mut Session,
    codec: crate::encode::Codec,
    mode: punktfunk_core::config::Mode,
    bitrate_kbps: u32,
    stop: &AtomicBool,
    probe_rx: &std::sync::mpsc::Receiver<ProbeShaped>,
    probe_result_tx: &tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    fec_target: &AtomicU8,
    probe_seq: bool,
) -> Result<()> {
    use crate::capture::Capturer;
    anyhow::ensure!(
        codec == crate::encode::Codec::H264,
        "the software source encodes H.264 only, and {codec:?} was negotiated"
    );
    let (w, h, fps) = (mode.width, mode.height, mode.refresh_hz.max(1));
    let mut capturer = crate::capture::SyntheticCapturer::new(w, h, fps);
    let mut frame = capturer.next_frame().context("first synthetic frame")?;
    let mut encoder =
        pf_encode::open_software_h264(frame.format, w, h, fps, u64::from(bitrate_kbps) * 1000)
            .context("open the software encoder")?;
    let interval = std::time::Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let mut frames = 0u64;
    while !stop.load(Ordering::SeqCst) {
        apply_fec_target(session, fec_target);
        service_probes(session, stop, probe_rx, probe_result_tx, probe_seq);
        encoder.submit(&frame).context("encode submit")?;
        while let Some(au) = encoder.poll().context("encode poll")? {
            let mut flags = u32::from(FLAG_PIC);
            if au.keyframe {
                flags |= u32::from(FLAG_SOF);
            }
            if session.submit_frame(&au.data, au.pts_ns, flags).is_err() {
                tracing::info!(frames, "software stream ended: the transport refused");
                return Ok(());
            }
            frames += 1;
        }
        std::thread::sleep(interval);
        frame = capturer.next_frame().context("synthetic frame")?;
    }
    tracing::info!(frames, "software stream complete");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn software_stream(
    _session: &mut Session,
    _codec: crate::encode::Codec,
    _mode: punktfunk_core::config::Mode,
    _bitrate_kbps: u32,
    _stop: &AtomicBool,
    _probe_rx: &std::sync::mpsc::Receiver<ProbeShaped>,
    _probe_result_tx: &tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    _fec_target: &AtomicU8,
    _probe_seq: bool,
) -> Result<()> {
    anyhow::bail!("the software source needs the software encoder, which is Linux-only")
}

/// Probe ceiling: 10 Gbps / 5 s. Above the session cap ([`MAX_BITRATE_KBPS`], 2 Gbps) so a
/// probe can show headroom past the rate a session will actually use.
const MAX_PROBE_KBPS: u32 = 10_000_000;
const MAX_PROBE_MS: u32 = 5_000;
/// One pump's send ceiling: a whole catch-up backlog in one batch overruns a ~400 KiB send
/// buffer. The rest follows on the next pump, which video shares.
const PROBE_PUMP_BYTES: u64 = 64 * 1024;

/// A speed-test burst in flight: zero-filled [`FLAG_PROBE`] AUs at `target_kbps` for
/// `duration_ms`, both clamped to `MAX_PROBE_*`. The send loop pumps it between AUs, so video
/// keeps flowing and the burst reads the headroom beside the live stream. A shaped request
/// ([`ProbeShaped`]) releases each period's bytes at once, in groups on a clock.
struct ProbeBurst {
    filler: Vec<u8>,
    target_kbps: u32,
    /// Pacing rate and its anchor: the budget is what has elapsed since `start`, so a pump
    /// that arrives late sends the backlog instead of forfeiting it.
    bytes_per_sec: u64,
    start: std::time::Instant,
    deadline: std::time::Instant,
    bytes_sent: u64,
    packets_sent: u32,
    /// Wire packets this burst offered, and those the send buffer refused. Counted per submit:
    /// video shares the loop now, so a session-stats delta would fold its shards in.
    wire_offered: u32,
    send_dropped: u32,
    /// `0` is the smooth train; otherwise `rate / burst_hz` bytes come due every period.
    burst_hz: u16,
    /// Bytes one pump may release of a shaped burst (`u64::MAX` = the whole backlog).
    group_bytes: u64,
    /// Clock the groups leave on; `0` = line rate.
    group_rate_bps: u64,
    group_next: std::time::Instant,
}

impl ProbeBurst {
    /// `None` = nothing to burst, answered with [`declined`]: an empty request, or a client
    /// without VIDEO_CAP_PROBE_SEQ. That client has one reassembly window and would drop probe
    /// frames as stale — and a burst in video's index space reads as a multi-thousand-frame
    /// loss once it ends.
    fn begin(req: impl Into<ProbeShaped>, probe_seq: bool) -> Option<ProbeBurst> {
        let req: ProbeShaped = req.into();
        if !probe_seq {
            tracing::info!(
                "declining speed-test probe: client predates VIDEO_CAP_PROBE_SEQ (its reassembler \
                 cannot window probe-space frames)"
            );
            return None;
        }
        let target_kbps = req.target_kbps.min(MAX_PROBE_KBPS);
        let duration_ms = req.duration_ms.min(MAX_PROBE_MS);
        if target_kbps == 0 || duration_ms == 0 {
            return None;
        }
        let bytes_per_sec = u64::from(target_kbps) * 125;
        // ≤16 KiB ≈ a dozen MTU shards; a 256 KiB AU overflowed a ~400 KiB send buffer on one submit.
        let chunk = (bytes_per_sec / 240).clamp(1200, 16 * 1024) as usize;
        let start = std::time::Instant::now();
        Some(ProbeBurst {
            filler: vec![0u8; chunk],
            target_kbps,
            bytes_per_sec,
            start,
            deadline: start + std::time::Duration::from_millis(u64::from(duration_ms)),
            bytes_sent: 0,
            packets_sent: 0,
            wire_offered: 0,
            send_dropped: 0,
            burst_hz: req.burst_hz,
            group_bytes: match req.group_bytes {
                0 => u64::MAX,
                b => u64::from(b),
            },
            group_rate_bps: u64::from(req.group_rate_kbps) * 1_000,
            group_next: start,
        })
    }

    fn period(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(1.0 / f64::from(self.burst_hz.max(1)))
    }

    /// Bytes the burst is allowed to have sent by now: elapsed × rate, held at the whole
    /// request so a late pump delivers the budget without overshooting it. A shaped burst
    /// is owed each period's bytes the moment the period starts.
    fn allowed_bytes(&self) -> u64 {
        let elapsed = self.start.elapsed().min(self.deadline - self.start);
        let whole = ((self.deadline - self.start).as_secs_f64() * self.bytes_per_sec as f64) as u64;
        if self.burst_hz == 0 {
            return (elapsed.as_secs_f64() * self.bytes_per_sec as f64) as u64;
        }
        let periods = (elapsed.as_secs_f64() / self.period().as_secs_f64()).floor() + 1.0;
        ((periods * self.bytes_per_sec as f64 / f64::from(self.burst_hz)) as u64).min(whole)
    }

    /// Send what the budget allows now, at most [`PROBE_PUMP_BYTES`], then hand the thread
    /// back. Sending the backlog is what keeps the measured rate true when video held the
    /// loop for an AU. A clocked group waits for its instant and leaves whole.
    fn pump(&mut self, session: &mut Session) {
        let now = std::time::Instant::now();
        let clocked = self.burst_hz > 0 && self.group_rate_bps > 0;
        if clocked && now < self.group_next {
            return;
        }
        let mut budget = self.allowed_bytes().min(self.bytes_sent + PROBE_PUMP_BYTES);
        if clocked {
            budget = budget.min(self.bytes_sent.saturating_add(self.group_bytes));
        }
        let before = self.bytes_sent;
        while self.bytes_sent < budget {
            // WouldBlock/ENOBUFS is part of what the probe measures (`send_dropped`) — keep going.
            if let Ok((offered, dropped)) = session.submit_probe_frame(&self.filler, now_ns()) {
                self.wire_offered += offered;
                self.send_dropped += dropped;
            }
            self.bytes_sent += self.filler.len() as u64;
            self.packets_sent += 1;
        }
        if clocked && self.bytes_sent > before {
            self.group_next = now
                + std::time::Duration::from_nanos(
                    (self.bytes_sent - before).saturating_mul(8_000_000_000) / self.group_rate_bps,
                );
        }
    }

    /// The requested duration has elapsed. Callers pump before they ask, so the last of the
    /// budget is already on the wire.
    fn expired(&self) -> bool {
        std::time::Instant::now() >= self.deadline
    }

    /// How long the caller may spend elsewhere: until the next filler comes due, zero while
    /// the budget is behind, and never past the burst's own deadline. A shaped burst with a
    /// backlog is due at its next group's instant, otherwise at the next period.
    fn next_due(&self) -> std::time::Duration {
        let now = std::time::Instant::now();
        let due = if self.burst_hz == 0 {
            self.start
                + std::time::Duration::from_secs_f64(
                    self.bytes_sent as f64 / self.bytes_per_sec as f64,
                )
        } else if self.bytes_sent < self.allowed_bytes() {
            if self.group_rate_bps > 0 {
                self.group_next
            } else {
                now
            }
        } else {
            let elapsed = now.saturating_duration_since(self.start);
            let periods = (elapsed.as_secs_f64() / self.period().as_secs_f64()).floor() as u32 + 1;
            self.start + self.period() * periods
        };
        due.saturating_duration_since(now)
            .min(self.deadline.saturating_duration_since(now))
    }

    /// Serve the whole burst on this thread, then report it. It pumps before it asks, so a
    /// thread woken past the deadline still sends what the window owed.
    pub(super) fn run(mut self, session: &mut Session, stop: &AtomicBool) -> ProbeResult {
        loop {
            self.pump(session);
            if self.expired() || stop.load(Ordering::SeqCst) {
                return self.finish();
            }
            std::thread::sleep(self.next_due().min(std::time::Duration::from_micros(200)));
        }
    }

    /// End the burst and report it. Both figures are what happened, so a burst cut short by
    /// `stop` or a teardown reports itself rather than the request.
    fn finish(self) -> ProbeResult {
        let duration_ms = self.start.elapsed().as_millis() as u32;
        let wire_packets_sent = self.wire_offered.saturating_sub(self.send_dropped);
        tracing::info!(
            target_kbps = self.target_kbps,
            duration_ms,
            bytes_sent = self.bytes_sent,
            au_count = self.packets_sent,
            wire_offered = self.wire_offered,
            wire_packets_sent,
            send_dropped = self.send_dropped,
            "speed-test probe burst complete"
        );
        ProbeResult {
            bytes_sent: self.bytes_sent,
            packets_sent: self.packets_sent,
            duration_ms,
            wire_packets_sent,
            send_dropped: self.send_dropped,
        }
    }
}

/// All-zero: the client reads it as a decline and keeps its negotiated ceiling.
pub(super) fn declined() -> ProbeResult {
    ProbeResult {
        bytes_sent: 0,
        packets_sent: 0,
        duration_ms: 0,
        wire_packets_sent: 0,
        send_dropped: 0,
    }
}

/// Pump the live burst, else take the next request. One slice per call: the send loop goes
/// straight back to its AUs, so the encode channel never fills and the capture pool keeps its
/// spare buffers. Never while a streamed AU is open.
fn service_burst(
    session: &mut Session,
    burst: &mut Option<ProbeBurst>,
    probe_rx: &std::sync::mpsc::Receiver<ProbeShaped>,
    probe_result_tx: &tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    probe_seq: bool,
) {
    match burst.as_mut() {
        Some(b) => {
            b.pump(session);
            if b.expired() {
                let done = burst.take().expect("armed on this branch");
                let _ = probe_result_tx.send(done.finish());
            }
        }
        None => {
            if let Ok(req) = probe_rx.try_recv() {
                match ProbeBurst::begin(req, probe_seq) {
                    Some(b) => *burst = Some(b),
                    None => {
                        let _ = probe_result_tx.send(declined());
                    }
                }
            }
        }
    }
}

/// A diagnostic session (`EXT_DELIVERY_PROBE_ONLY`): the punched data plane serves every
/// probe in full and nothing else — no display, encoder, launch or seat — until the client
/// closes, [`PROBE_ONLY_MAX`] passes, or the probes have cost [`PROBE_ONLY_BYTES`]. Past the
/// byte cap the rest are declined; the per-request clamps still apply to each.
pub(crate) fn probe_only_stream(common: &mut StreamCommon, probe_seq: bool) -> Result<()> {
    const PROBE_ONLY_MAX: std::time::Duration = std::time::Duration::from_secs(60);
    const PROBE_ONLY_BYTES: u64 = 2 << 30;
    tracing::info!("diagnostic session: probes only, no pipeline");
    let started = std::time::Instant::now();
    while !common.stop.load(Ordering::SeqCst) && started.elapsed() < PROBE_ONLY_MAX {
        if common.session.stats().bytes_sent >= PROBE_ONLY_BYTES {
            while common.ends.probe_rx.try_recv().is_ok() {
                let _ = common.ends.probe_result_tx.send(declined());
            }
        } else {
            service_probes(
                &mut common.session,
                &common.stop,
                &common.ends.probe_rx,
                &common.ends.probe_result_tx,
                probe_seq,
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    tracing::info!(
        bytes = common.session.stats().bytes_sent,
        secs = started.elapsed().as_secs(),
        "diagnostic session over"
    );
    Ok(())
}

/// Serve pending speed-test requests between frames, blocking here for each burst. The
/// synthetic and software sources hold no capture buffers and owe no deadline; the
/// virtual-display path pumps [`ProbeBurst`] from its send loop instead.
fn service_probes(
    session: &mut Session,
    stop: &AtomicBool,
    probe_rx: &std::sync::mpsc::Receiver<ProbeShaped>,
    probe_result_tx: &tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    probe_seq: bool,
) {
    while let Ok(req) = probe_rx.try_recv() {
        let result = match ProbeBurst::begin(req, probe_seq) {
            Some(burst) => burst.run(session, stop),
            None => declined(),
        };
        let _ = probe_result_tx.send(result);
    }
}

/// Seal one AU and send it under [`send_pacing`](crate::send_pacing): the profile's
/// allowance leaves first (`Burst`/`Capped`: 10 ms at the pace rate, clamped to
/// [16 KiB, 256 KiB]; `Smooth`: one super-packet; `PUNKTFUNK_PACE_BURST_KB` pins it),
/// `Capped` on its clock; overflow spreads at the pace rate in groups of up to 64 packets.
/// An unpaced line-rate burst overruns the kernel tx buffer → EAGAIN → freeze until the
/// next keyframe.
///
/// The pace rate is ~3× the live encoder bitrate, or the link rate the client's ramp proved
/// for a pinned stream — the overflow's wire time at that rate is the budget
/// ([`crate::send_pacing::native_budget`], [`MAX_PACE_SPREAD`]-bounded). `0` = deadline-only
/// spread (`PUNKTFUNK_PACE_FACTOR=0`, or bitrate not yet known).
fn paced_submit(
    session: &mut Session,
    data: &[u8],
    pts_ns: u64,
    flags: u32,
    frame_index: u32,
    deadline: std::time::Instant,
    pacing: &mut crate::send_pacing::Pacing,
) -> Result<PaceStat> {
    if pacing.pace_rate_bps == 0 {
        // The deadline-only spread is sized on the whole frame.
        let wires = session
            .seal_frame_at(data, pts_ns, flags, frame_index)
            .map_err(|e| anyhow!("seal_frame: {e:?}"))?;
        return pace_sealed(session, wires, deadline, pacing.burst_cap, pacing);
    }
    // Chunks go out as they are sealed; the first packet leaves after one chunk, not the frame.
    let total = session.frame_wire_len(data.len()).max(1);
    let burst = pacing.burst_bytes(total);
    let mut pace = FramePace::new(total, burst, pacing);
    let sealed =
        session.seal_frame_chunks_at(data, pts_ns, flags, frame_index, &mut |chunk, send| {
            pace.chunk(chunk, send)
        });
    let sock_ns = pace.sock_ns;
    let stat = pace.stat();
    session.note_sock_ns(sock_ns);
    sealed.map_err(|e| anyhow!("seal_frame: {e:?}"))?;
    Ok(stat)
}

/// One frame's pacing across the chunks [`Session::seal_frame_chunks_at`] hands over:
/// one microburst (on `Capped`'s clock when there is one), then groups of up to 64
/// packets — or `group_bytes`, whichever binds — released on a schedule at the pace rate
/// from the first paced group. A sub-floor wait is skipped and caught up by a later
/// group, so the rate holds in ~half-millisecond steps however small the chunks are.
struct FramePace<'a> {
    /// bits/s past the burst. The spread cap is folded in as a floor on the rate.
    rate_bps: u64,
    burst_left: usize,
    group_bytes: usize,
    clock: Option<&'a mut crate::send_pacing::GroupClock>,
    paced_bytes: u64,
    pace_start: Option<std::time::Instant>,
    started: Option<std::time::Instant>,
    paced: bool,
    sock_ns: u64,
}

/// Packets of the next group: at least one, at most [`FramePace::GROUP`], no more than
/// `max_bytes` past the first.
fn group_len(pkts: &[&[u8]], max_bytes: usize) -> usize {
    let mut cum = 0usize;
    let mut n = 0usize;
    for p in pkts.iter().take(FramePace::GROUP) {
        if n > 0 && cum.saturating_add(p.len()) > max_bytes {
            break;
        }
        cum += p.len();
        n += 1;
    }
    n
}

impl<'a> FramePace<'a> {
    const GROUP: usize = 64;
    const SLEEP_FLOOR: std::time::Duration = std::time::Duration::from_micros(500);

    fn new(total: usize, burst: usize, pacing: &'a mut crate::send_pacing::Pacing) -> Self {
        let overflow = total.saturating_sub(burst) as u64;
        let cap_ns = pacing
            .max_spread
            .min(crate::send_pacing::MAX_PACE_SPREAD)
            .as_nanos()
            .max(1) as u64;
        let floor_bps = overflow.saturating_mul(8_000_000_000) / cap_ns;
        FramePace {
            rate_bps: pacing.pace_rate_bps.max(floor_bps).max(1),
            burst_left: burst,
            group_bytes: pacing.group_bytes(),
            clock: pacing.clock.as_mut(),
            paced_bytes: 0,
            pace_start: None,
            started: None,
            paced: false,
            sock_ns: 0,
        }
    }

    fn chunk(
        &mut self,
        chunk: &[Vec<u8>],
        send: &mut dyn FnMut(&[&[u8]]) -> punktfunk_core::Result<usize>,
    ) -> punktfunk_core::Result<()> {
        self.started.get_or_insert_with(std::time::Instant::now);
        let mut refs: Vec<&[u8]> = chunk.iter().map(|w| w.as_slice()).collect();
        crate::send_pacing::inject_video_drop(&mut refs);
        let mut rest: &[&[u8]] = &refs;
        while !rest.is_empty() {
            let (group, tail) = rest.split_at(group_len(rest, self.group_bytes));
            rest = tail;
            let bytes: usize = group.iter().map(|p| p.len()).sum();
            if self.burst_left > 0 {
                // The group that crosses the cap still bursts, as `pace_frame` does.
                self.burst_left = self.burst_left.saturating_sub(bytes);
                if let Some(clock) = self.clock.as_deref_mut() {
                    let ahead = clock.wait(bytes, std::time::Instant::now());
                    if ahead >= Self::SLEEP_FLOOR {
                        std::thread::sleep(ahead);
                    }
                }
            } else {
                self.paced = true;
                let start = *self.pace_start.get_or_insert_with(std::time::Instant::now);
                let due = start
                    + std::time::Duration::from_nanos(
                        self.paced_bytes.saturating_mul(8_000_000_000) / self.rate_bps,
                    );
                if let Some(ahead) = due.checked_duration_since(std::time::Instant::now()) {
                    if ahead >= Self::SLEEP_FLOOR {
                        std::thread::sleep(ahead);
                    }
                }
                self.paced_bytes += bytes as u64;
            }
            let t0 = std::time::Instant::now();
            send(group)?;
            self.sock_ns += t0.elapsed().as_nanos() as u64;
        }
        Ok(())
    }

    /// First packet → last packet, seal gaps included.
    fn stat(&self) -> PaceStat {
        PaceStat {
            spread_us: self
                .started
                .map(|s| s.elapsed().as_micros() as u32)
                .unwrap_or(0),
            paced: self.paced,
        }
    }
}

/// Pace already-sealed wires. Shared with the streamed-AU path ([`handle_chunk`]), whose
/// `burst` is what is left of the AU's allowance; `None` takes the profile's.
fn pace_sealed(
    session: &mut Session,
    wires: Vec<Vec<u8>>,
    deadline: std::time::Instant,
    burst: Option<usize>,
    pacing: &mut crate::send_pacing::Pacing,
) -> Result<PaceStat> {
    let mut refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
    crate::send_pacing::inject_video_drop(&mut refs);
    let result = pace_wires(&refs, deadline, burst, pacing, &mut |chunk| {
        session.send_sealed(chunk)
    });
    drop(refs);
    session.reclaim_wires(wires);
    match result {
        Ok((stat, sock_ns)) => {
            session.note_sock_ns(sock_ns);
            Ok(stat)
        }
        Err(e) => Err(anyhow!("send_sealed: {e:?}")),
    }
}

/// Burst then pace `refs` through `send` ([`crate::send_pacing`]). `burst` `None` = the
/// profile's allowance. Returns the spread and the time spent inside `send`: sleeps between
/// chunks stay excluded, so `sock_ns` is pure send_gso/sendmmsg time.
fn pace_wires(
    refs: &[&[u8]],
    deadline: std::time::Instant,
    burst: Option<usize>,
    pacing: &mut crate::send_pacing::Pacing,
    send: &mut dyn FnMut(&[&[u8]]) -> punktfunk_core::Result<usize>,
) -> punktfunk_core::Result<(PaceStat, u64)> {
    let wire_bytes: usize = refs.iter().map(|p| p.len()).sum();
    let burst_bytes = burst.unwrap_or_else(|| pacing.burst_bytes(wire_bytes));
    let cfg = crate::send_pacing::PaceCfg {
        burst_bytes: Some(burst_bytes),
        chunk: crate::send_pacing::ChunkPolicy::Adaptive { base: 16, max: 64 },
        sleep_floor: std::time::Duration::from_micros(500),
    };
    let overflow_bytes = wire_bytes.saturating_sub(burst_bytes) as u64;
    let budget = crate::send_pacing::native_budget(
        deadline,
        pacing.pace_rate_bps,
        overflow_bytes,
        pacing.max_spread,
    );
    let mut sock_ns = 0u64;
    let stat =
        crate::send_pacing::pace_frame(refs, budget, &cfg, pacing.clock.as_mut(), |chunk| {
            let t0 = std::time::Instant::now();
            let r = send(chunk).map(|_| ());
            sock_ns += t0.elapsed().as_nanos() as u64;
            r
        })?;
    Ok((stat, sock_ns))
}

/// What every stream source takes from its session: the punched session, its schedule, the
/// stream thread's [`StreamEnds`], the [`SessionShared`] values and the session's identity.
/// [`SessionContext`] and [`SynthAbrContext`] both carry one.
///
/// [`StreamEnds`]: super::wiring::StreamEnds
/// [`SessionShared`]: super::wiring::SessionShared
pub(crate) struct StreamCommon {
    pub(crate) session: Session,
    pub(crate) mode: punktfunk_core::Mode,
    pub(crate) seconds: u32,
    pub(crate) stop: Arc<AtomicBool>,
    /// Set on `QUIT_CODE`. Display lease skips keep-alive linger for a user stop.
    pub(crate) quit: Arc<AtomicBool>,
    /// [`crate::events::SessionEndReason`] latch for the session summary; first write wins.
    pub(crate) end_reason: Arc<std::sync::atomic::AtomicU8>,
    /// Session totals for the summary; the encode loop notes every bitrate it runs at.
    pub(crate) counters: Arc<crate::session_status::SessionCounters>,
    pub(crate) ends: super::wiring::StreamEnds,
    pub(crate) shared: super::wiring::SessionShared,
    /// Total wire budget (kbps): video + FEC + framing + audio reservation. PyroWave is identity.
    pub(crate) bitrate_kbps: u32,
    pub(crate) audio_reserved_kbps: u32,
    pub(crate) shard_payload: u16,
    pub(crate) timing_conn: Option<super::link::SessionLink>,
    /// Without this, a mid-session probe consumes video indexes the gap detector cannot see.
    pub(crate) probe_seq: bool,
    pub(crate) stats: Arc<StatsRecorder>,
    pub(crate) client_label: String,
    pub(crate) bringup: Arc<crate::bringup::Trace>,
    /// A clone of the data socket for the sender's kernel-queue probe; `None` on the web plane.
    pub(crate) wire_sock: Option<std::net::UdpSocket>,
    pub(crate) codec: crate::encode::Codec,
    /// Per-session handles the management routes act on; published to the registry.
    pub(crate) controls: crate::session_status::SessionControls,
    pub(crate) client_name: Option<String>,
    /// 10-bit alone does not imply HDR: 10-bit SDR is its own path.
    pub(crate) hdr: bool,
    pub(crate) bit_depth: u8,
    pub(crate) chroma: crate::encode::ChromaFormat,
}

/// Owned per-session inputs for [`virtual_stream`]: the [`StreamCommon`] plus what a display
/// needs. The whole context moves onto the stream thread.
pub(super) struct SessionContext {
    pub(super) common: StreamCommon,
    pub(super) compositor: crate::vdisplay::Compositor,
    /// Per-instance, not via `PUNKTFUNK_GAMESCOPE_NODE` — two sessions must not overwrite each other.
    pub(super) gamescope_route: Option<crate::vdisplay::GamescopeRoute>,
    /// `Hello::bitrate_kbps == 0`. PyroWave re-resolves on a mid-stream mode switch; an explicit rate stays.
    pub(super) bitrate_auto: bool,
    pub(super) conn: super::link::SessionLink,
    pub(super) cursor_forward: bool,
    pub(super) streamed_au: bool,
    /// `false` = single-slice. TV-SoC decoders (Amlogic) wedge on multi-slice.
    pub(super) multi_slice: bool,
    pub(super) launch: Option<String>,
    pub(super) launch_target: Option<crate::library::LaunchTarget>,
    /// This session's launch record, claimed before prep and the launch hold.
    pub(super) launch_claim: Option<crate::launchreg::Claim>,
    /// [`crate::session_launch::Prepared::stamp`].
    pub(super) launch_stamp: Option<f64>,
    pub(super) launch_owner: crate::session_launch::LaunchOwner,
    /// Where this session's launch outcome goes; the control task writes it to
    /// the client ([`punktfunk_core::quic::LaunchOutcome`]).
    pub(super) launch_outcome: crate::gamelease::OutcomeTx,
    /// Threaded into the EDID CTA HDR block before `create` so host apps tone-map to the client's panel.
    pub(super) client_hdr: Option<pf_frame::HdrMeta>,
    /// Admitted by `mode_conflict: join`: share the live display instead of creating one.
    pub(super) join_live: bool,
    /// A joiner's view and fit ([`SessionPlan::reframe_to`](crate::session_plan::SessionPlan::reframe_to)).
    pub(super) reframe_to: Option<(punktfunk_core::video_fit::VideoFit, (u32, u32))>,
    /// The encoder's framing, published for the input thread.
    pub(super) frame_map: super::input::FrameMap,
    /// The session gamescope's X displays, published for the clipboard.
    #[cfg(target_os = "linux")]
    pub(super) gamescope_xwayland: pf_clipboard::GamescopeXwayland,
    pub(super) resize_ms: Arc<AtomicU32>,
    #[cfg(target_os = "linux")]
    pub(super) input_tx: std::sync::mpsc::SyncSender<super::input::ClientInput>,
    /// Isolated gamescope spawn identity. `None` = shared planes. See `design/gamescope-multiuser.md`.
    #[cfg(target_os = "linux")]
    pub(super) isolation: Option<crate::vdisplay::SessionIsolation>,
    #[cfg(target_os = "linux")]
    pub(super) input_route: super::input::InputRoute,
    #[cfg(target_os = "linux")]
    pub(super) inj_shared_tx: std::sync::mpsc::Sender<punktfunk_core::input::InputEvent>,
    #[cfg(target_os = "linux")]
    pub(super) inj_session_tx: Option<std::sync::mpsc::Sender<punktfunk_core::input::InputEvent>>,
}

/// The virtual-display session: bring up, then tick until the client leaves.
pub(super) fn virtual_stream(ctx: SessionContext, prepared: Option<PreparedDisplay>) -> Result<()> {
    boost_thread_priority(true);
    StreamState::new(ctx, prepared)?.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::ProbeRequest;

    /// A host session onto an in-process link, and the client that reads it back.
    fn loopback_sessions() -> (Session, Session) {
        use punktfunk_core::config::{Config, Role};
        let (host_tp, client_tp) = punktfunk_core::transport::loopback_pair(0, 0);
        (
            Session::new(
                Config::defaults(Role::Host),
                punktfunk_core::session::MediaV2::default(),
                Box::new(host_tp),
            )
            .expect("host session"),
            Session::new(
                Config::defaults(Role::Client),
                punktfunk_core::session::MediaV2::default(),
                Box::new(client_tp),
            )
            .expect("client session"),
        )
    }

    /// One pump sends one slice, not the burst: the budget is what has elapsed. Across a
    /// burst's worth of pumps the slices still add up to the bytes the client asked for.
    #[test]
    fn a_burst_spends_its_byte_budget_across_pumps() {
        let (mut host, _client) = loopback_sessions();
        let req = ProbeRequest {
            target_kbps: 8_000,
            duration_ms: 200,
        };
        let mut burst = ProbeBurst::begin(req, true).expect("a capable client arms a burst");
        let chunk = burst.filler.len() as u64;
        burst.pump(&mut host);
        assert!(
            burst.bytes_sent <= 2 * chunk,
            "the first pump sends a slice, not the burst: {} bytes",
            burst.bytes_sent
        );
        let mut pumps = 1;
        while !burst.expired() {
            std::thread::sleep(std::time::Duration::from_millis(5));
            burst.pump(&mut host);
            pumps += 1;
        }
        burst.pump(&mut host); // the last budgeted bytes, as the send loop does before it finishes
        assert!(pumps > 5, "a 200 ms burst pumps many times: {pumps}");
        let budget = 200 * 8_000 / 8; // duration_ms × kbps / 8 = bytes
        let r = burst.finish();
        assert!(
            r.bytes_sent >= budget * 8 / 10 && r.bytes_sent <= budget + chunk,
            "delivered {} of a {budget} byte budget",
            r.bytes_sent
        );
        assert_eq!(r.send_dropped, 0, "an in-process link refuses nothing");
        assert!(
            u64::from(r.wire_packets_sent) >= u64::from(r.packets_sent),
            "each filler AU is at least one wire packet"
        );
    }

    /// Video AUs keep leaving the host through a burst, and the burst's report counts only
    /// its own filler: the client's probe counter matches it exactly.
    #[test]
    fn video_flows_through_a_burst_and_stays_out_of_its_report() {
        let (mut host, mut client) = loopback_sessions();
        let req = ProbeRequest {
            target_kbps: 8_000,
            duration_ms: 100,
        };
        let mut burst = ProbeBurst::begin(req, true).expect("a capable client arms a burst");
        let video = vec![7u8; 16 * 1024];
        let (mut video_frames, mut sent_video) = (0u32, 0u32);
        let drain = |client: &mut Session, video_frames: &mut u32| {
            while let Ok(f) = client.poll_frame() {
                if f.flags & FLAG_PROBE as u32 == 0 {
                    *video_frames += 1;
                }
            }
        };
        while !burst.expired() {
            std::thread::sleep(std::time::Duration::from_millis(5));
            burst.pump(&mut host);
            host.submit_frame(&video, now_ns(), (FLAG_PIC | FLAG_SOF) as u32)
                .expect("a video AU during the burst");
            sent_video += 1;
            drain(&mut client, &mut video_frames);
        }
        burst.pump(&mut host);
        drain(&mut client, &mut video_frames);
        let r = burst.finish();
        assert!(
            sent_video > 3 && video_frames >= sent_video - 1,
            "video kept flowing: {video_frames} of {sent_video} AUs arrived"
        );
        let st = client.stats();
        assert_eq!(
            u64::from(r.wire_packets_sent),
            st.probe_packets_received,
            "the report counts probe packets, never the video shards beside them"
        );
        assert!(
            st.packets_received > st.probe_packets_received,
            "video shared the wire with the burst"
        );
    }

    /// A burst reports once and leaves nothing armed, and a client that cannot window probe
    /// indexes is declined without arming anything.
    #[test]
    fn a_burst_disarms_itself_and_reports_once() {
        let (mut host, _client) = loopback_sessions();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<ProbeShaped>();
        let (res_tx, mut res_rx) = tokio::sync::mpsc::unbounded_channel::<ProbeResult>();
        let mut burst: Option<ProbeBurst> = None;
        let req = ProbeRequest {
            target_kbps: 4_000,
            duration_ms: 20,
        };
        req_tx.send(req.into()).unwrap();
        service_burst(&mut host, &mut burst, &req_rx, &res_tx, true);
        assert!(burst.is_some(), "the request arms a burst");
        assert!(res_rx.try_recv().is_err(), "no result until the burst ends");
        std::thread::sleep(std::time::Duration::from_millis(25));
        service_burst(&mut host, &mut burst, &req_rx, &res_tx, true);
        assert!(burst.is_none(), "an expired burst leaves nothing armed");
        let r = res_rx.try_recv().expect("the finished burst reports");
        assert!(r.bytes_sent > 0 && r.duration_ms >= 20, "{r:?}");
        assert!(res_rx.try_recv().is_err(), "one result per request");
        req_tx.send(req.into()).unwrap();
        service_burst(&mut host, &mut burst, &req_rx, &res_tx, false);
        assert!(burst.is_none(), "an old client arms nothing");
        assert_eq!(res_rx.try_recv().expect("a decline"), declined());
    }

    /// A thread that reaches its burst only after the deadline still sends the window, so
    /// the client never reads a descheduled host as one that sent nothing.
    #[test]
    fn a_burst_run_late_still_sends_its_window() {
        let (mut host, _client) = loopback_sessions();
        let req = ProbeRequest {
            target_kbps: 5_000,
            duration_ms: 25,
        };
        let burst = ProbeBurst::begin(req, true).expect("a capable client arms a burst");
        std::thread::sleep(std::time::Duration::from_millis(40));
        let r = burst.run(&mut host, &AtomicBool::new(false));
        let budget = 25 * 5_000 / 8;
        assert!(
            r.bytes_sent * 100 >= budget * 90,
            "sent {} of a {budget} byte window",
            r.bytes_sent
        );
    }

    /// Teardown mid-burst: the report is what went out, not what was asked for.
    #[test]
    fn a_burst_cut_short_reports_what_it_sent() {
        let (mut host, _client) = loopback_sessions();
        let req = ProbeRequest {
            target_kbps: 8_000,
            duration_ms: 5_000,
        };
        let mut burst = ProbeBurst::begin(req, true).expect("a capable client arms a burst");
        std::thread::sleep(std::time::Duration::from_millis(20));
        burst.pump(&mut host);
        let r = burst.finish();
        assert!(
            r.duration_ms >= 20 && r.duration_ms < 5_000,
            "the window is what elapsed: {} ms",
            r.duration_ms
        );
        assert!(
            r.bytes_sent > 0 && r.bytes_sent < 5_000 * 8_000 / 8,
            "the bytes are what went out: {}",
            r.bytes_sent
        );
    }

    #[test]
    fn reconfig_allowed_gates_gamescope_and_per_client_mode() {
        use crate::vdisplay::Compositor::{Gamescope, Hyprland, Kwin, Mutter, Wlroots};
        assert!(!reconfig_allowed(Some(Gamescope), false, false));
        assert!(!reconfig_allowed(Some(Gamescope), true, false));
        assert!(!reconfig_allowed(Some(Kwin), true, false));
        assert!(!reconfig_allowed(Some(Mutter), true, false));
        assert!(!reconfig_allowed(None, true, false));
        for c in [Kwin, Mutter, Wlroots, Hyprland] {
            assert!(
                reconfig_allowed(Some(c), false, false),
                "{c:?} should allow live reconfigure"
            );
        }
        assert!(reconfig_allowed(None, false, false));
    }

    #[test]
    fn reconfig_allowed_rejects_a_monitor_mirror_on_every_backend() {
        use crate::vdisplay::Compositor::{Hyprland, Kwin, Mutter, Wlroots};
        for c in [Kwin, Mutter, Wlroots, Hyprland] {
            assert!(
                reconfig_allowed(Some(c), false, false),
                "{c:?} without a pin should still allow live reconfigure"
            );
            assert!(
                !reconfig_allowed(Some(c), false, true),
                "{c:?} mirroring a physical head must reject a resize"
            );
        }
    }
}
