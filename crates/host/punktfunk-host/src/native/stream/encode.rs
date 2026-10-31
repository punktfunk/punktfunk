//! The per-tick data path: capture, submit, poll into the send thread, the encode-stall watch,
//! the encode-cadence adaptation, and the pacing sleep. The drain after the loop is here too.

use super::state::{Flow, StreamState, Tick};
use super::*;
use crate::encode_recovery::{
    grab_frame, on_submit_error, reset_stalled_encoder, run_parked_stage, MAX_ENCODER_RESETS,
};
use crate::stats_recorder::DriverSample;

// ~20 net behind-frames (≈0.3 s) escalates; warmup skips the first ~1 s of bring-up.
const ESCALATE_BEHIND_SCORE: u32 = 20;
const BEHIND_SCORE_CAP: u32 = 60;
const CADENCE_WARMUP_FRAMES: u64 = 60;
const DEGRADE_BEHIND_SCORE: u32 = 10;
// ~5 s clean at 120 fps earns one wind-back. Backoff 1 → 5 → 25 min; never a permanent latch.
const DEESCALATE_CLEAN_FRAMES: u32 = 600;
pub(super) const DEESCALATE_BACKOFF_START: std::time::Duration = std::time::Duration::from_secs(60);
const DEESCALATE_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(25 * 60);
const CADENCE_LOG_MIN_GAP: std::time::Duration = std::time::Duration::from_secs(5);
// The cadence line's chain percentiles cover this many ticks: ~1 s at 60 fps.
const CHAIN_WINDOW: usize = 64;

/// The newest [`CHAIN_WINDOW`] submit+poll chains, µs, in a fixed ring.
pub(super) struct ChainWindow {
    us: [u32; CHAIN_WINDOW],
    len: usize,
    at: usize,
}

impl Default for ChainWindow {
    fn default() -> Self {
        Self {
            us: [0; CHAIN_WINDOW],
            len: 0,
            at: 0,
        }
    }
}

impl ChainWindow {
    fn push(&mut self, us: u32) {
        self.us[self.at] = us;
        self.at = (self.at + 1) % CHAIN_WINDOW;
        self.len = (self.len + 1).min(CHAIN_WINDOW);
    }

    /// Over a stack copy, so the ring keeps its order. `None` before the first tick.
    fn percentile(&self, q: f64) -> Option<u32> {
        let mut v = self.us;
        (self.len > 0).then(|| percentile(&mut v[..self.len], q))
    }
}

/// The frames per second a source period implies, rounded.
fn source_fps(src_period_ns: Option<u64>) -> Option<u32> {
    src_period_ns
        .filter(|&p| p > 0)
        .map(|p| (1e9 / p as f64).round() as u32)
}

/// What the encoder reads, as the frame carries it: the payload and a dmabuf's modifier
/// (0 = LINEAR). Packed RGB in a dmabuf is converted on the encoder's GPU.
fn capture_path(frame: &crate::capture::CapturedFrame) -> (&'static str, Option<u64>) {
    match &frame.payload {
        pf_frame::FramePayload::Cpu(_) => ("cpu", None),
        pf_frame::FramePayload::Pixelless => ("pixelless", None),
        #[cfg(target_os = "linux")]
        pf_frame::FramePayload::Cuda(_) => ("cuda", None),
        #[cfg(target_os = "linux")]
        pf_frame::FramePayload::Dmabuf(d) => ("dmabuf", Some(d.modifier)),
        #[cfg(target_os = "windows")]
        pf_frame::FramePayload::D3d11(_) => ("d3d11", None),
    }
}

/// Tag a wave-boundary AU with [`USER_FLAG_RECOVERY_POINT`](punktfunk_core::packet::USER_FLAG_RECOVERY_POINT).
///
/// `ir_wave_pos` counts frames since the last IDR/wave start. An IDR re-phases it to 0 and is
/// itself a clean anchor, so it is never additionally marked. Every `period`-th non-IDR AU is a
/// boundary — the client lifts its post-loss freeze on the SECOND such mark.
fn mark_recovery_boundary(ir_wave_pos: &mut u32, is_keyframe: bool, period: u32) -> bool {
    if is_keyframe {
        *ir_wave_pos = 0;
        false
    } else {
        *ir_wave_pos += 1;
        if *ir_wave_pos >= period {
            *ir_wave_pos = 0;
            true
        } else {
            false
        }
    }
}

/// Escalated sessions flag on any net behind-frame; being escalated alone does not latch a cap.
fn encode_behind_cadence(escalated: bool, behind_score: u32, degrade_at: u32) -> bool {
    behind_score >= degrade_at || (escalated && behind_score > 0)
}

/// Observed source period, clamped to [interval, 4×]. No estimate yet keeps the interval.
fn cadence_budget(
    interval: std::time::Duration,
    src_period_ns: Option<u64>,
) -> std::time::Duration {
    match src_period_ns {
        Some(p) => std::time::Duration::from_nanos(p).clamp(interval, interval * 4),
        None => interval,
    }
}

/// Encode pressure from the host's submit+poll chain, not the consumed-frame clock: a chain at
/// the bottleneck still reads ≥80% of a budget it stretched. Pipelined retrieve takes the wait off
/// the loop but blocks submit on the input copy, so its chain reads high while cadence holds;
/// there only a chain that fills the budget is pressure, or the escalation could never unwind.
fn encode_chain_pressure(budget: std::time::Duration, chain_ns: u64, pipelined: bool) -> bool {
    let chain = u128::from(chain_ns);
    if pipelined {
        chain >= budget.as_nanos()
    } else {
        chain.saturating_mul(5) >= budget.as_nanos().saturating_mul(4)
    }
}

/// The wire flags for one access unit: picture, keyframe, and the recovery marks the client
/// lifts its post-loss freeze on.
///
/// The single point where the encoder's answer to an RFI is visible, so the per-minute link
/// line is counted here: a clean anchor P, or the start of a wave (`recovery_point` without
/// `recovery_close`). The host's own periodic boundary marking is not a wave start.
#[allow(clippy::too_many_arguments)]
fn au_flags(
    caps: &crate::encode::EncoderCaps,
    ir_wave_pos: &mut u32,
    keyframe: bool,
    recovery_point: bool,
    recovery_close: bool,
    recovery_anchor: bool,
    chunk_aligned: bool,
    acked: bool,
    link: &crate::link_health::LinkCounters,
) -> u32 {
    link.note_recovery_au(recovery_anchor, acked, recovery_point && !recovery_close);
    let mut flags = if keyframe {
        (FLAG_PIC | FLAG_SOF) as u32
    } else {
        FLAG_PIC as u32
    };
    if caps.intra_refresh_recovery
        && caps.intra_refresh_period > 0
        && mark_recovery_boundary(ir_wave_pos, keyframe, caps.intra_refresh_period)
    {
        flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_POINT;
    }
    if recovery_point {
        flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_POINT;
    }
    if recovery_close {
        flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_CLOSE;
    }
    if recovery_anchor {
        flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_ANCHOR;
    }
    if chunk_aligned {
        flags |= punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED;
    }
    flags
}

/// Source stamps on a display faster than the stream, moved onto the content's cadence
/// ([`pf_frame::pace::Restamp`]). Every ten seconds it logs how far each frame's gap moved
/// from the one before, on the producer's stamps and on the corrected ones.
#[cfg(target_os = "linux")]
pub(super) struct SourceStamps {
    restamp: pf_frame::pace::Restamp,
    change: pf_frame::pace::GapChange,
    tick_ns: u64,
    since: std::time::Instant,
}

#[cfg(target_os = "linux")]
impl SourceStamps {
    const REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

    pub(super) fn new(tick_ns: u64) -> Self {
        Self {
            restamp: pf_frame::pace::Restamp::new(tick_ns),
            change: pf_frame::pace::GapChange::default(),
            tick_ns,
            since: std::time::Instant::now(),
        }
    }

    fn apply(&mut self, pts_ns: u64) -> u64 {
        let out = self.restamp.apply(pts_ns);
        self.change.note(pts_ns, out);
        if self.since.elapsed() >= Self::REPORT_EVERY {
            let (raw, corrected) = self.change.take();
            tracing::info!(
                tick_us = self.tick_ns / 1000,
                raw_gap_change_us = raw / 1000,
                pts_gap_change_us = corrected / 1000,
                "source stamp cadence on a display faster than the stream"
            );
            self.since = std::time::Instant::now();
        }
        out
    }
}

impl StreamState {
    /// Grab the newest frame ([`grab_frame`]), run any recovery rung the capturer's ladder
    /// chose, and on a capture error rebuild ([`Self::on_capture_lost`]). `Ok(None)` ends the
    /// session cleanly.
    pub(super) fn capture_tick(&mut self) -> Result<Option<Tick>> {
        let measure = self.facts.perf || self.facts.stats.is_armed();
        let t_cap = std::time::Instant::now();
        let telemetry = self.live.enc.telemetry();
        if let Some(t) = &telemetry {
            self.shared
                .driver_dropped
                .store(t.dropped_total, Ordering::Relaxed);
        }
        let cap_result = grab_frame(&mut *self.live.capturer, &mut self.live.asks, telemetry);
        let cap_us = if measure {
            t_cap.elapsed().as_micros() as u32
        } else {
            0
        };
        if self.facts.perf {
            self.diag.st_cap.push(cap_us);
        }
        let mut was_reset = false;
        run_parked_stage(
            &mut *self.live.capturer,
            &mut self.live.asks,
            &mut *self.live.enc,
            &mut self.clock.watchdog,
            || was_reset = true,
        );
        if was_reset {
            self.clock.after_reset();
        }
        let mut repeat = false;
        match cap_result {
            Ok(Some(f)) => self.on_frame(f, t_cap),
            Ok(None) => {
                self.diag.diag_repeat += 1;
                repeat = true;
                // The source stalled past its own cadence: the estimate no longer describes it.
                self.src_period_ns = None;
            }
            Err(e) => {
                if !self.on_capture_lost(e)? {
                    return Ok(None);
                }
            }
        }
        Ok(Some(Tick {
            t_cap,
            cap_us,
            repeat,
            measure,
        }))
    }

    /// A fresh frame from the capturer: provenance bookkeeping, the stamp correction on a
    /// display faster than the stream, the source-cadence estimate, and the park re-arm.
    fn on_frame(&mut self, f: crate::capture::CapturedFrame, t_cap: std::time::Instant) {
        // Only a real SOURCE frame is evidence of source progress: a cursor-only
        // regeneration re-encodes the previous desktop image at a new pointer
        // position — encoded and sent like any frame, but never fed to the cadence
        // estimate, new-frame diagnostics, or capture-rebuild reset (a regenerated
        // cursor over one stashed texture is how a dead path used to look healthy).
        let source = f.provenance.origin == pf_frame::FrameOrigin::Source;
        self.live.frame = f;
        #[cfg(target_os = "linux")]
        if source && let Some(s) = self.live.restamp.as_mut() {
            self.live.frame.pts_ns = s.apply(self.live.frame.pts_ns);
        }
        if source {
            self.diag.diag_new += 1;
        } else {
            self.diag.diag_regen += 1;
        }
        // Source-cadence estimate: `t_cap` on the frame-driven path is taken right after
        // `wait_arrival` wakes, so real-frame deltas track the game's actual delivery spacing.
        // Deltas past 8×interval are a gap/hitch (mid-rebuild, alt-tab), not cadence — skipped.
        if source {
            if let Some(prev) = self.last_real_cap {
                let d = t_cap.duration_since(prev).as_nanos() as u64;
                if d <= self.live.interval.as_nanos() as u64 * 8 {
                    self.src_period_ns = Some(match self.src_period_ns {
                        Some(e) => (e as i64 + (d as i64 - e as i64) / 8) as u64,
                        None => d,
                    });
                }
            }
            self.last_real_cap = Some(t_cap);
        }
        if source {
            self.capture_rebuilds = 0; // a delivered SOURCE frame clears the loss counter
        }
        // Re-arm the park schedule for a (re)built display: pin the seat pointer to the
        // streamed surface (see `park_seat_pointer`). Not gamescope — its nested seat owns
        // the pointer and its cursor comes from the XFixes source regardless of seat position.
        #[cfg(target_os = "linux")]
        if self.display.compositor != pf_vdisplay::Compositor::Gamescope
            && self.display.parked_display
                != Some((self.live.cur_node_id, self.live.cur_display_gen))
        {
            self.display.parked_display = Some((self.live.cur_node_id, self.live.cur_display_gen));
            self.display.park_attempts = 0;
            self.display.next_park_at = std::time::Instant::now();
            self.display.moves_at_arm = self.facts.counters.pointer_moves.load(Ordering::Relaxed);
        }
    }

    /// Every 2 s under `PUNKTFUNK_PERF`: the new/repeat/regen split and the stage percentiles.
    pub(super) fn log_diag(&mut self) {
        if !(self.facts.perf && self.diag.diag_at.elapsed() >= std::time::Duration::from_secs(2)) {
            return;
        }
        let secs = self.diag.diag_at.elapsed().as_secs_f64();
        tracing::info!(
            new_fps = format!("{:.0}", self.diag.diag_new as f64 / secs),
            repeat_fps = format!("{:.0}", self.diag.diag_repeat as f64 / secs),
            regen_fps = format!("{:.0}", self.diag.diag_regen as f64 / secs),
            "capture diag: NEW frames from the source vs REPEATS vs cursor REGENS (low new_fps \
             at high send rate ⇒ the source isn't producing frames, not an encode stall; \
             regens alone are a cursor over a frozen image)"
        );
        let wait_max = self.diag.st_wait.iter().copied().max().unwrap_or(0);
        tracing::info!(
            queue_us_p50 = percentile(&mut self.diag.st_queue, 0.50),
            queue_us_p99 = percentile(&mut self.diag.st_queue, 0.99),
            cap_us_p50 = percentile(&mut self.diag.st_cap, 0.50),
            cap_us_p99 = percentile(&mut self.diag.st_cap, 0.99),
            submit_us_p50 = percentile(&mut self.diag.st_submit, 0.50),
            submit_us_p99 = percentile(&mut self.diag.st_submit, 0.99),
            wait_us_p50 = percentile(&mut self.diag.st_wait, 0.50),
            wait_us_p99 = percentile(&mut self.diag.st_wait, 0.99),
            wait_us_max = wait_max,
            "stage perf (µs/call): queue=delivery→submit cap=try_latest(ring+convert) submit=encode_picture wait=lock_bitstream(sched+ASIC)"
        );
        self.diag.st_cap.clear();
        self.diag.st_submit.clear();
        self.diag.st_wait.clear();
        self.diag.st_queue.clear();
        self.diag.diag_new = 0;
        self.diag.diag_repeat = 0;
        self.diag.diag_regen = 0;
        self.diag.diag_at = std::time::Instant::now();
    }

    /// Submit this tick's frame (or wait on the driver's owed AUs), then poll every ready AU
    /// into the send thread. `Continue` = the submit failed and the tick is spent on the
    /// backoff; `Break` = the send thread is gone.
    pub(super) fn encode_and_send(&mut self, tick: Tick) -> Result<Flow> {
        self.encode_chain_ns = 0;
        let Tick {
            t_cap: _,
            cap_us,
            repeat,
            measure,
        } = tick;
        let hdr_meta = self
            .live
            .capturer
            .colour()
            .hdr_meta
            .map(|m| self.facts.client_hdr.unwrap_or(m));
        self.live.enc.set_hdr_meta(hdr_meta);
        if hdr_meta != self.last_hdr_meta {
            self.last_hdr_meta = hdr_meta;
            self.hdr_meta_owed = true;
        }
        let submit_ns = now_ns();
        // SyntheticCapturer counts from 0, not the epoch — fall back to "now".
        let age_ns = submit_ns.saturating_sub(self.live.frame.pts_ns);
        let plausible = self.live.frame.pts_ns > 0
            && self.live.frame.pts_ns <= submit_ns
            && age_ns < 10_000_000_000;
        let (capture_ns, queue_us) = if !repeat && plausible {
            (self.live.frame.pts_ns, (age_ns / 1000) as u32)
        } else {
            (submit_ns, 0)
        };
        if self.facts.perf && !repeat {
            self.diag.st_queue.push(queue_us);
        }
        let t_submit = std::time::Instant::now();
        let wire_index = self
            .clock
            .au_seq
            .wrapping_add(self.clock.inflight.len() as u32);
        // A pixel-less frame's encoder (the Windows driver) already holds `owed` access units —
        // waited for after a fresh frame, never on a repeat; every other backend takes this
        // tick's frame.
        let au_wait = if repeat {
            t_submit
        } else {
            t_submit + self.live.interval
        };
        let owed = crate::send_pacing::owed_aus(&self.live.frame, &mut *self.live.enc, au_wait);
        // Every backend gets the floor, the pixel-less driver encoder too: without it the
        // driver's AMF/QSV/NVENC keep referencing frames the client never confirmed.
        let floor = self.loss_mode.floor(
            self.shared.lossy_window.swap(false, Ordering::Relaxed),
            self.shared.acked.load(Ordering::Relaxed),
            wire_index,
            t_submit,
        );
        self.live.enc.set_reference_floor(floor);
        let submitted = match owed {
            Some(_) => Ok(()),
            None => self.live.enc.submit_indexed(&self.live.frame, wire_index),
        };
        if let Err(e) = submitted {
            let mut was_reset = false;
            let backoff = on_submit_error(&mut self.clock.watchdog, e, self.live.interval, || {
                reset_stalled_encoder(&mut *self.live.enc, || was_reset = true)
            })?;
            if was_reset {
                self.clock.after_reset();
            }
            self.next = std::time::Instant::now() + backoff;
            std::thread::sleep(backoff);
            return Ok(Flow::Continue);
        }
        let submit_elapsed = t_submit.elapsed();
        let submit_us = if measure {
            submit_elapsed.as_micros() as u32
        } else {
            0
        };
        if self.facts.perf {
            self.diag.st_submit.push(submit_us);
        }
        self.next = if frame_driven_enabled() && self.live.capturer.feed().is_some() {
            std::time::Instant::now() + self.live.interval
        } else {
            self.next + self.live.interval
        };
        for _ in 0..owed.unwrap_or(1) {
            self.clock.inflight.push_back((capture_ns, submit_ns));
        }
        let stamps = Stamps {
            queue_us,
            cap_us,
            submit_us,
            submit_elapsed,
            repeat,
            measure,
            owed: owed.is_some(),
        };
        let mut send_gone = false;
        let mut poll_err: Option<anyhow::Error> = None;
        while !self.clock.inflight.is_empty() {
            if self.facts.streamed_wire && self.live.enc.supports_chunked_poll() {
                match self.poll_chunked(&stamps) {
                    Polled::Au => continue,
                    Polled::Nothing => break,
                    Polled::SendGone => {
                        send_gone = true;
                        break;
                    }
                    Polled::Failed(e) => {
                        poll_err = Some(e);
                        break;
                    }
                }
            }
            match self.poll_whole(&stamps) {
                Polled::Au => {}
                Polled::Nothing => break,
                Polled::SendGone => {
                    send_gone = true;
                    break;
                }
                Polled::Failed(e) => {
                    poll_err = Some(e);
                    break;
                }
            }
        }
        if send_gone {
            // The send thread ended, or stopped taking while the loop was leaving: the session
            // ending. Otherwise it died on an error it logged, which is not a clean end.
            if !self.leaving() {
                anyhow::bail!("video send thread stopped");
            }
            return Ok(Flow::Break);
        }
        self.check_encode_stall(poll_err)?;
        Ok(Flow::Next)
    }

    /// Fold the time this AU spent waiting on the encoder into the tick's longest submit+poll
    /// chain — the backend-neutral pressure input `adapt_to_cadence` reads. `waited` is the
    /// poll calls alone: on the chunked path the sends between them are the wire's cost, not
    /// the encoder's. Driver-owned AUs carry no host submit, so they never count.
    fn note_encode_chain(&mut self, st: &Stamps, waited: std::time::Duration) {
        if st.owed {
            return;
        }
        self.encode_chain_ns = self.encode_chain_ns.max(
            st.submit_elapsed
                .saturating_add(waited)
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64,
        );
    }

    /// The AU-level fields for the AU just taken, from its `inflight` stamps
    /// `(capture, submit)`. On the driver the host submits nothing: the stages are
    /// the driver's own, and its per-AU present time (`pts_ns`) beats the tick's clock, which
    /// would give every AU of a burst the same one.
    fn au_meta(
        &self,
        st: &Stamps,
        (cap_ns, sub_ns): (u64, u64),
        pts_ns: u64,
        flags: u32,
        wait_us: u32,
    ) -> AuMeta {
        let d = if st.owed {
            driver_stages(&*self.live.enc)
        } else {
            AuStages::host((now_ns().saturating_sub(sub_ns) / 1000) as u32, st.queue_us)
        };
        AuMeta {
            capture_ns: if st.owed && pts_ns > 0 {
                pts_ns
            } else {
                cap_ns
            },
            epoch: self.live.epoch,
            stream: 0,
            flags,
            frame_index: self.clock.au_seq,
            encode_us: d.encode_us,
            queue_us: d.queue_us,
            cap_us: st.cap_us,
            submit_us: st.submit_us,
            wait_us,
            repeat: d.repeat.unwrap_or(st.repeat),
            was_measured: st.measure,
            driver: d.driver,
        }
    }

    /// Stream one AU's chunks as they land; an AU in one chunk goes out whole. `Au` = a whole
    /// AU went out (the caller polls again while owed frames remain); `Nothing` = the encoder
    /// has no more output this tick.
    fn poll_chunked(&mut self, st: &Stamps) -> Polled {
        let t_wait = std::time::Instant::now();
        let mut waited = std::time::Duration::ZERO;
        let mut first_chunk_us = 0u32;
        let mut flags = 0u32;
        loop {
            let t_poll = std::time::Instant::now();
            let polled = self.live.enc.poll_chunk();
            waited += t_poll.elapsed();
            self.note_encode_chain(st, waited);
            let c = match polled {
                Ok(Some(c)) => c,
                Ok(None) => return Polled::Nothing,
                Err(e) => return Polled::Failed(e),
            };
            self.clock.watchdog.on_au();
            // A FIRST while the previous AU's LAST never came (the driver dropped its
            // tail, or a reset abandoned it): close that frame on the wire first, or
            // this AU is spliced onto a truncated prefix under its own flags.
            if c.first && self.clock.wire_frame_open {
                if self.clock.inflight.len() > 1 {
                    self.clock.inflight.pop_front();
                }
                self.clock.au_seq = self.clock.au_seq.wrapping_add(1);
            }
            self.clock.wire_frame_open = !c.last;
            if c.first {
                first_chunk_us = t_wait.elapsed().as_micros() as u32;
                let caps = self.live.enc.caps();
                flags = au_flags(
                    &caps,
                    &mut self.ir_wave_pos,
                    c.keyframe,
                    c.recovery_point,
                    c.recovery_close,
                    c.recovery_anchor,
                    c.chunk_aligned,
                    self.loss_mode.is_on(),
                    &self.facts.counters.link,
                );
                self.send_hdr_meta(c.keyframe);
                self.facts.bringup.mark("first_au");
            }
            let last = c.last;
            let inflight = *self.clock.inflight.front().expect("inflight non-empty");
            let wait_total_us = t_wait.elapsed().as_micros() as u32;
            let wait_us = if st.measure { wait_total_us } else { 0 };
            let meta = self.au_meta(st, inflight, c.pts_ns, flags, wait_us);
            let encode_us = meta.encode_us;
            let msg = ChunkMsg {
                data: c.data,
                first: c.first,
                last,
                meta,
            };
            if !self.send.send(msg.into_msg(), || self.leaving()) {
                return Polled::SendGone;
            }
            if last {
                self.clock.inflight.pop_front();
                self.clock.au_seq = self.clock.au_seq.wrapping_add(1);
                self.sent += 1;
                if self.facts.perf {
                    self.diag.st_wait.push(wait_total_us);
                    if self.sent % 120 == 0 {
                        tracing::info!(
                            first_slice_us = first_chunk_us,
                            encode_us,
                            "streamed AU (sampled): first slice handed to send at \
                             first_slice_us; encode finished at encode_us"
                        );
                    }
                }
                return Polled::Au;
            }
        }
    }

    /// Poll one whole AU into the send thread.
    fn poll_whole(&mut self, st: &Stamps) -> Polled {
        let t_wait = std::time::Instant::now();
        let polled = self.live.enc.poll();
        let wait_elapsed = t_wait.elapsed();
        self.note_encode_chain(st, wait_elapsed);
        let wait_us = if st.measure {
            wait_elapsed.as_micros() as u32
        } else {
            0
        };
        if self.facts.perf {
            self.diag.st_wait.push(wait_us);
        }
        let au = match polled {
            Ok(Some(au)) => au,
            Ok(None) => return Polled::Nothing,
            Err(e) => return Polled::Failed(e),
        };
        self.clock.watchdog.on_au();
        let inflight = self.clock.inflight.pop_front().expect("inflight non-empty");
        let caps = self.live.enc.caps();
        let flags = au_flags(
            &caps,
            &mut self.ir_wave_pos,
            au.keyframe,
            au.recovery_point,
            au.recovery_close,
            au.recovery_anchor,
            au.chunk_aligned,
            self.loss_mode.is_on(),
            &self.facts.counters.link,
        );
        self.send_hdr_meta(au.keyframe);
        let msg = FrameMsg {
            meta: self.au_meta(st, inflight, au.pts_ns, flags, wait_us),
            data: au.data,
        };
        self.facts.bringup.mark("first_au");
        if !self.send.send(SendMsg::Frame(msg), || self.leaving()) {
            return Polled::SendGone;
        }
        self.clock.au_seq = self.clock.au_seq.wrapping_add(1);
        self.sent += 1;
        Polled::Au
    }

    /// HDR metadata rides a datagram on every keyframe, and on the first AU after it changes.
    fn send_hdr_meta(&mut self, keyframe: bool) {
        let Some(m) = self.last_hdr_meta else {
            return;
        };
        if keyframe || self.hdr_meta_owed {
            let _ = self
                .facts
                .conn
                .send_datagram(punktfunk_core::quic::encode_hdr_meta_datagram(
                    &crate::encode::hdr_meta_to_wire(m),
                ));
            self.hdr_meta_owed = false;
        }
    }

    /// A stop or the link's close: the loop is leaving, so a hand-off waits for neither.
    fn leaving(&self) -> bool {
        self.facts.life.stopping() || self.facts.conn.close_reason().is_some()
    }

    /// A poll failure, or owed frames with no AU for the stall window: rebuild the encoder in
    /// place, up to [`MAX_ENCODER_RESETS`] times, and wait out each reset's backoff.
    fn check_encode_stall(&mut self, poll_err: Option<anyhow::Error>) -> Result<()> {
        let stalled = self
            .clock
            .watchdog
            .stalled(self.clock.inflight.len(), 1, self.live.interval);
        let why = match (&poll_err, stalled) {
            (Some(e), _) => format!("poll failed: {e:#}"),
            (None, Some(why)) => why,
            (None, None) => return Ok(()),
        };
        let mut was_reset = false;
        let recovered = self.clock.watchdog.recover(self.live.interval, || {
            reset_stalled_encoder(&mut *self.live.enc, || was_reset = true)
        });
        if was_reset {
            self.clock.after_reset();
        }
        let Some(backoff) = recovered else {
            return Err(poll_err.unwrap_or_else(|| anyhow!("{why}")))
                .context("encoder stalled — in-place rebuild unavailable or exhausted");
        };
        tracing::warn!(reset = self.clock.watchdog.resets(), max = MAX_ENCODER_RESETS, %why,
            "encode stall detected — encoder rebuilt in place, forcing an IDR");
        // The reset's backoff, as a failed submit takes it: a poll that fails again at once
        // must not spend the whole budget in as many ticks.
        self.next = std::time::Instant::now() + backoff;
        std::thread::sleep(backoff);
        Ok(())
    }

    /// Pipelined-retrieve escalation on a sustained behind-cadence score, wound back after a
    /// clean run with a growing backoff; pipelined retrieve is judged on a full-budget chain
    /// (see [`encode_chain_pressure`]). Also publishes the cadence verdict the ABR climb gate
    /// reads, and logs its flips ([`Self::log_cadence_flip`]). With `PUNKTFUNK_IDD_ADAPTIVE`
    /// off it does none of this.
    pub(super) fn adapt_to_cadence(&mut self) {
        let p = &mut self.pipelining;
        if !p.adaptive {
            return;
        }
        p.depth_frames += 1;
        if p.depth_frames <= CADENCE_WARMUP_FRAMES {
            return;
        }
        self.diag
            .chain
            .push((self.encode_chain_ns / 1_000).min(u64::from(u32::MAX)) as u32);
        let budget = cadence_budget(self.live.interval, self.src_period_ns);
        let schedule_behind =
            std::time::Instant::now() >= self.next + (budget - self.live.interval);
        let encode_pressure =
            encode_chain_pressure(budget, self.encode_chain_ns, p.pipelined_active);
        let behind = schedule_behind || encode_pressure;
        p.behind_score = if behind {
            (p.behind_score + 1).min(BEHIND_SCORE_CAP)
        } else {
            p.behind_score.saturating_sub(1)
        };
        let escalated = p.pipelined_active || p.deescalating;
        let degraded = encode_behind_cadence(escalated, p.behind_score, DEGRADE_BEHIND_SCORE);
        self.shared
            .cadence_degraded
            .store(degraded, Ordering::Relaxed);
        self.shared
            .cadence_behind_score
            .store(p.behind_score, Ordering::Relaxed);
        if degraded != self.diag.was_degraded {
            self.log_cadence_flip(degraded, encode_pressure, budget);
            self.diag.was_degraded = degraded;
        }
        let p = &mut self.pipelining;
        if p.deescalating {
            if !self.live.enc.set_pipelined(false) {
                p.deescalating = false;
                p.pipelined_active = false;
                p.pipeline_asked = false;
                tracing::info!(
                    "encoder pipelined retrieve de-escalated — sync retrieve (and \
                     sub-frame streaming, where armed) restored; re-monitoring cadence"
                );
                p.behind_score = 0;
                p.depth_frames = 0;
                p.ahead_run = 0;
            }
        } else if p.behind_score >= ESCALATE_BEHIND_SCORE && !p.pipeline_asked {
            p.pipeline_asked = true;
            p.pipelined_active = self.live.enc.set_pipelined(true);
            if p.pipelined_active {
                tracing::info!(
                    "encoder pipelined retrieve escalated — encode can't hold cadence; \
                     the encode wait moves off the loop until cadence holds clean \
                     (latency trade for throughput)"
                );
            }
            p.behind_score = 0;
            p.ahead_run = 0;
        } else if escalated {
            p.ahead_run = if behind { 0 } else { p.ahead_run + 1 };
            if p.ahead_run >= DEESCALATE_CLEAN_FRAMES
                && p.deescalate_not_before
                    .is_none_or(|t| std::time::Instant::now() >= t)
            {
                p.ahead_run = 0;
                p.deescalate_not_before = Some(std::time::Instant::now() + p.deescalate_backoff);
                p.deescalate_backoff = (p.deescalate_backoff * 5).min(DEESCALATE_BACKOFF_MAX);
                tracing::info!(
                    "cadence held clean while escalated — winding the pipelined \
                     retrieve back (latency recovery; costs one IDR)"
                );
                p.deescalating = true;
            }
        }
    }

    /// A verdict flip, one line per [`CADENCE_LOG_MIN_GAP`] at most, with what it rests on:
    /// this tick's encode chain and the last [`CHAIN_WINDOW`] ticks', the source period, the
    /// path the encoder reads, and the producer pool's withheld peak since the last line. On
    /// the frame-driven path a chain near the source period means the loop sets that period.
    fn log_cadence_flip(
        &mut self,
        degraded: bool,
        encode_pressure: bool,
        budget: std::time::Duration,
    ) {
        let now = std::time::Instant::now();
        if self
            .diag
            .last_cadence_log
            .is_some_and(|t| now.duration_since(t) < CADENCE_LOG_MIN_GAP)
        {
            self.diag.cadence_flips_suppressed += 1;
            return;
        }
        let p = &self.pipelining;
        let (behind_score, pipelined) = (p.behind_score, p.pipelined_active);
        let escalated = p.pipelined_active || p.deescalating;
        let (capture, capture_modifier) = capture_path(&self.live.frame);
        let capture_format = self.live.frame.format;
        let pool = self.live.capturer.feed().and_then(|f| f.pool_use());
        let chain = &self.diag.chain;
        macro_rules! cadence_line {
            ($msg:literal) => {
                tracing::info!(
                    behind_score,
                    escalated,
                    pipelined,
                    encode_pressure,
                    chain_us = self.encode_chain_ns / 1_000,
                    chain_us_p50 = chain.percentile(0.50),
                    chain_us_p90 = chain.percentile(0.90),
                    budget_us = budget.as_micros() as u64,
                    interval_us = self.live.interval.as_micros() as u64,
                    src_period_us = self.src_period_ns.map(|p| p / 1_000),
                    src_fps = source_fps(self.src_period_ns),
                    capture,
                    capture_format = ?capture_format,
                    capture_modifier,
                    pool_depth = pool.map(|u| u.depth),
                    pool_withheld_peak = pool.map(|u| u.withheld_peak),
                    flips_suppressed = self.diag.cadence_flips_suppressed,
                    $msg
                )
            };
        }
        if degraded {
            cadence_line!("encode behind cadence — ABR climbs will be refused until it recovers");
            // Name any other encoder on the engine; Windows pauses Instant Replay there.
            crate::encoder_sessions::on_behind_cadence();
        } else {
            cadence_line!("encode cadence recovered — ABR climbs allowed again");
        }
        self.diag.last_cadence_log = Some(now);
        self.diag.cadence_flips_suppressed = 0;
    }

    /// Wait for the next tick ([`crate::send_pacing::wait_next_tick`]) on this session's
    /// measured source cadence.
    pub(super) fn sleep_to_next(&mut self, t_cap: std::time::Instant, repeat: bool) {
        crate::send_pacing::wait_next_tick(
            &mut *self.live.capturer,
            &mut *self.live.enc,
            &self.live.frame,
            &mut self.pace,
            &mut self.next,
            (!repeat).then_some(t_cap),
            self.live.interval,
            self.src_period_ns,
        );
    }

    /// After the loop: poll what the encoder still owes into the send thread.
    /// Pipelined retrieve must be wound back first — a probing `poll` would
    /// return `None` on the first in-flight AU and strand the tail.
    pub(super) fn drain(&mut self) {
        self.live.enc.set_pipelined(false);
        // A stopped session still ships what it encoded; only a sender that stopped taking
        // ends the drain early.
        let give_up_at = std::time::Instant::now() + super::send::SEND_JOIN_GRACE;
        while let Some((cap_ns, sub_ns)) = self.clock.inflight.pop_front() {
            let Ok(Some(au)) = self.live.enc.poll() else {
                break;
            };
            let flags = if au.keyframe {
                (FLAG_PIC | FLAG_SOF) as u32
            } else {
                FLAG_PIC as u32
            };
            let encode_us = (now_ns().saturating_sub(sub_ns) / 1000) as u32;
            let msg = FrameMsg {
                data: au.data,
                meta: AuMeta {
                    capture_ns: cap_ns,
                    epoch: self.live.epoch,
                    stream: 0,
                    flags,
                    frame_index: self.clock.au_seq,
                    encode_us,
                    queue_us: 0,
                    cap_us: 0,
                    submit_us: 0,
                    wait_us: 0,
                    repeat: false,
                    was_measured: false,
                    driver: None,
                },
            };
            if !self.send.send(SendMsg::Frame(msg), || {
                std::time::Instant::now() >= give_up_at
            }) {
                break;
            }
            self.clock.au_seq = self.clock.au_seq.wrapping_add(1);
            self.sent += 1;
        }
    }
}

/// What one AU's `queue_us`/`encode_us` mean on its message: the host's own stamps, or the
/// driver's ([`DriverSample::queue_encode_us`]), whose full sample rides along for the stats.
struct AuStages {
    queue_us: u32,
    encode_us: u32,
    driver: Option<DriverSample>,
    /// The driver's own word on whether this AU repeats the last source frame. `None` leaves
    /// it to the tick, which is right for an encoder the loop feeds.
    repeat: Option<bool>,
}

impl AuStages {
    fn host(encode_us: u32, queue_us: u32) -> AuStages {
        AuStages {
            queue_us,
            encode_us,
            driver: None,
            repeat: None,
        }
    }
}

/// The driver's stages for the AU just taken. Unmeasured before its first AU.
fn driver_stages(enc: &dyn crate::encode::Encoder) -> AuStages {
    let telemetry = enc.telemetry();
    let sample = DriverSample::from_telemetry(telemetry.as_ref());
    let (queue_us, encode_us) = sample.queue_encode_us();
    AuStages {
        queue_us,
        encode_us,
        driver: Some(sample),
        repeat: telemetry.map(|t| t.au_repeat),
    }
}

/// This tick's stage timings, stamped onto every AU it produces.
struct Stamps {
    queue_us: u32,
    cap_us: u32,
    submit_us: u32,
    /// Always captured (perf sampling aside): the submit half of the encode-pressure chain.
    submit_elapsed: std::time::Duration,
    repeat: bool,
    measure: bool,
    /// The driver already held the AUs: their own present time beats the tick's clock.
    owed: bool,
}

enum Polled {
    /// One AU went to the send thread.
    Au,
    /// No output ready.
    Nothing,
    SendGone,
    Failed(anyhow::Error),
}

#[cfg(test)]
mod tests {
    /// An in-place reset closes a frame the wire had open and stamps the keyframe cooldown,
    /// as a swap does.
    #[test]
    fn an_in_place_reset_closes_the_open_frame() {
        let mut f = super::super::swap::tests::fake();
        f.st.live.enc = Box::new(Resets);
        f.st.clock.wire_frame_open = true;
        let au_seq = f.st.clock.au_seq;
        f.st.check_encode_stall(Some(anyhow::anyhow!("poll failed")))
            .expect("one reset of the budget");
        assert_eq!(
            f.st.clock.au_seq,
            au_seq + 1,
            "the open frame was not closed"
        );
        assert!(!f.st.clock.wire_frame_open);
        assert!(f.st.clock.inflight.is_empty());
        assert!(
            f.st.clock.last_forced_idr.is_some(),
            "the reset's IDR is not on the clock"
        );
    }

    /// An encoder whose in-place reset always works.
    struct Resets;

    impl crate::encode::Encoder for Resets {
        fn submit(&mut self, _frame: &crate::capture::CapturedFrame) -> anyhow::Result<()> {
            Ok(())
        }
        fn poll(&mut self) -> anyhow::Result<Option<crate::encode::EncodedFrame>> {
            Ok(None)
        }
        fn flush(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn reset(&mut self) -> bool {
            true
        }
    }

    /// A poll failure the encoder survives in place costs the reset's backoff, as a submit
    /// failure does: back-to-back failures must not spend every reset in as many ticks.
    #[test]
    fn a_failed_poll_waits_out_the_reset_backoff() {
        let mut f = super::super::swap::tests::fake();
        f.st.live.enc = Box::new(Resets);
        let t0 = std::time::Instant::now();
        f.st.check_encode_stall(Some(anyhow::anyhow!("poll failed")))
            .expect("one reset of the budget");
        assert!(
            f.st.next >= t0 + std::time::Duration::from_millis(100),
            "the next tick did not wait out the backoff"
        );
    }
    use super::*;

    /// The ring answers over its newest entries only, and nothing before the first tick.
    #[test]
    fn the_chain_window_reads_its_newest_ticks() {
        let mut w = ChainWindow::default();
        assert_eq!(w.percentile(0.5), None);
        w.push(7);
        assert_eq!(w.percentile(0.9), Some(7));
        for us in 0..CHAIN_WINDOW as u32 {
            w.push(1_000 + us);
        }
        assert_eq!(w.percentile(0.0), Some(1_000), "the 7 rolled out");
        assert_eq!(w.percentile(1.0), Some(1_000 + CHAIN_WINDOW as u32 - 1));
        w.push(5);
        assert_eq!(w.percentile(0.0), Some(5));
        assert_eq!(
            w.percentile(1.5 / CHAIN_WINDOW as f64),
            Some(1_001),
            "the oldest entry went: reading left the ring's order alone"
        );
    }

    #[test]
    fn a_source_period_reads_as_whole_frames_per_second() {
        let rows = [
            (Some(20_394_000), Some(49)),
            (Some(16_666_667), Some(60)),
            (Some(0), None),
            (None, None),
        ];
        for (period_ns, fps) in rows {
            assert_eq!(source_fps(period_ns), fps, "{period_ns:?}");
        }
    }

    #[test]
    fn a_cpu_frame_names_its_path_without_a_modifier() {
        let frame = crate::capture::CapturedFrame {
            width: 2,
            height: 2,
            pts_ns: 0,
            format: pf_frame::PixelFormat::Bgrx,
            payload: pf_frame::FramePayload::Cpu(vec![0; 16]),
            cursor: None,
            provenance: Default::default(),
        };
        assert_eq!(capture_path(&frame), ("cpu", None));
    }

    #[test]
    fn an_escalated_but_caught_up_encoder_stops_refusing_climbs() {
        const DEGRADE: u32 = 10;
        assert!(!encode_behind_cadence(false, 0, DEGRADE));
        assert!(!encode_behind_cadence(false, 9, DEGRADE));
        assert!(encode_behind_cadence(false, 10, DEGRADE));
        assert!(encode_behind_cadence(true, 1, DEGRADE));
        assert!(!encode_behind_cadence(true, 0, DEGRADE));
    }

    /// 80% of budget spent in the submit+poll chain is pressure — boundary inclusive —
    /// and a chain that stretched the budget to itself cannot hide behind it.
    #[test]
    fn encode_chain_pressure_fires_at_eighty_percent_of_budget() {
        let ms = std::time::Duration::from_millis;
        assert!(!encode_chain_pressure(ms(10), 3_000_000, false));
        assert!(encode_chain_pressure(ms(10), 8_000_000, false));
        assert!(encode_chain_pressure(ms(10), 13_000_000, false));
        assert!(!encode_chain_pressure(
            ms(16) + std::time::Duration::from_micros(667),
            13_000_000,
            false
        ));
        assert!(encode_chain_pressure(ms(13), 13_000_000, false));
    }

    /// Pipelined, a blocking 7 ms copy at 120 Hz holds cadence; only a full budget is behind.
    #[test]
    fn a_pipelined_chain_is_pressure_only_at_the_full_budget() {
        let budget = std::time::Duration::from_micros(8333);
        assert!(encode_chain_pressure(budget, 7_000_000, false));
        assert!(!encode_chain_pressure(budget, 7_000_000, true));
        assert!(!encode_chain_pressure(budget, 8_332_000, true));
        assert!(encode_chain_pressure(budget, 8_333_000, true));
        assert!(encode_chain_pressure(budget, 12_000_000, true));
    }

    #[test]
    fn the_behind_budget_tracks_the_source_not_the_negotiated_refresh() {
        let interval = std::time::Duration::from_micros(8333);
        let us = |d: std::time::Duration| d.as_micros() as u64;

        assert_eq!(cadence_budget(interval, None), interval);
        assert_eq!(
            us(cadence_budget(interval, Some(16_600_000))),
            16_600,
            "a 60 fps source's frames have 16.6 ms of real budget"
        );
        assert_eq!(us(cadence_budget(interval, Some(8_333_000))), 8_333);
        assert_eq!(cadence_budget(interval, Some(4_000_000)), interval);
        assert_eq!(cadence_budget(interval, Some(500_000_000)), interval * 4);
    }

    #[test]
    fn recovery_marks_land_every_period_and_rephase_at_idr() {
        let period = 4;
        let mut pos = 0u32;
        let marks: Vec<bool> = (0..10)
            .map(|_| mark_recovery_boundary(&mut pos, false, period))
            .collect();
        assert_eq!(
            marks,
            vec![false, false, false, true, false, false, false, true, false, false]
        );

        let mut pos = 0u32;
        assert!(!mark_recovery_boundary(&mut pos, false, period));
        assert!(!mark_recovery_boundary(&mut pos, false, period));
        assert!(!mark_recovery_boundary(&mut pos, true, period));
        assert!(!mark_recovery_boundary(&mut pos, false, period));
        assert!(!mark_recovery_boundary(&mut pos, false, period));
        assert!(!mark_recovery_boundary(&mut pos, false, period));
        assert!(mark_recovery_boundary(&mut pos, false, period));
    }
}
