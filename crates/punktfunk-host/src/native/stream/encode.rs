//! The per-tick data path: capture, submit, poll into the send thread, the encode-stall watch,
//! the IDD pipeline-depth adaptation, and the pacing sleep. The drain after the loop is here too.

use super::recovery::{reset_stalled_encoder, run_loop_stage};
use super::state::{Flow, StreamState, Tick};
use super::*;
use crate::encode_recovery::MAX_ENCODER_RESETS;
use crate::stats_recorder::DriverSample;

// ~20 net behind-frames (≈0.3 s) escalates; warmup skips the first ~1 s of bring-up.
const DEPTH_ESCALATE: u32 = 20;
const DEPTH_BEHIND_CAP: u32 = 60;
const DEPTH_WARMUP_FRAMES: u64 = 60;
const DEPTH_DEGRADE: u32 = 10;
// ~5 s clean at 120 fps earns one wind-back. Backoff 1 → 5 → 25 min; never a permanent latch.
const DEESCALATE_CLEAN_FRAMES: u32 = 600;
pub(super) const DEESCALATE_BACKOFF_START: std::time::Duration = std::time::Duration::from_secs(60);
const DEESCALATE_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(25 * 60);
const CADENCE_LOG_MIN_GAP: std::time::Duration = std::time::Duration::from_secs(5);

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

/// Depth-1 by default: depth-2 holds a ready AU a whole interval unpolled (~13 ms extra at 60 fps).
/// Escalate to the capturer's max only when cadence cannot hold at depth-1 (GPU contention).
/// `PUNKTFUNK_IDD_ADAPTIVE=0` pins the capturer's full depth. Off when max depth is already 1.
fn idd_adaptive_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| pf_host_config::env_on("PUNKTFUNK_IDD_ADAPTIVE").unwrap_or(true))
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

/// How long past `next` the arrival wait runs before a repeat goes out: half an interval
/// with no measured source cadence, else until 1.25× the source period after the last
/// submit. A steady slower source gets no repeat between its own frames; a stalled one
/// re-sends after that same wait.
fn keepalive_wait(
    interval: std::time::Duration,
    src_period_ns: Option<u64>,
) -> std::time::Duration {
    let half = interval.mul_f32(0.5);
    match src_period_ns {
        Some(p) => std::time::Duration::from_nanos(p)
            .mul_f32(1.25)
            .saturating_sub(interval)
            .max(half),
        None => half,
    }
}

/// 80% of the cadence budget spent inside the host's submit+poll chain is encode
/// pressure: activation follows the measured encoder cost, not the consumed-frame
/// clock — a chain at the bottleneck still reads ≥80% of a budget it stretched.
fn encode_chain_pressure(budget: std::time::Duration, chain_ns: u64) -> bool {
    u128::from(chain_ns).saturating_mul(5) >= budget.as_nanos().saturating_mul(4)
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
    link: &crate::link_health::LinkCounters,
) -> u32 {
    link.note_recovery_au(recovery_anchor, recovery_point && !recovery_close);
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
    /// Pull the newest frame, run any recovery rung the capturer's ladder chose, and on a
    /// capture error rebuild ([`Self::on_capture_lost`]). `Ok(None)` ends the session cleanly.
    pub(super) fn capture_tick(&mut self) -> Result<Option<Tick>> {
        let measure = self.perf || self.stats.is_armed();
        let t_cap = std::time::Instant::now();
        let telemetry = self.enc.telemetry();
        if let Some(t) = &telemetry {
            self.driver_dropped
                .store(t.dropped_total, Ordering::Relaxed);
        }
        self.capturer.observe_encoder(telemetry);
        let cap_result = self.capturer.try_latest();
        let cap_us = if measure {
            t_cap.elapsed().as_micros() as u32
        } else {
            0
        };
        if self.perf {
            self.st_cap.push(cap_us);
        }
        // A recovery rung the capturer's ladder chose but this loop must run (the encoder is
        // ours). Its outcome goes straight back; a reset forfeited every in-flight AU.
        if let Some(stage) = self.capturer.take_pending_stage() {
            let outcome = run_loop_stage(stage, &mut self.enc, &mut self.inflight);
            self.capturer.stage_done(stage, outcome);
            self.watchdog.restart();
        }
        let mut repeat = false;
        match cap_result {
            Ok(Some(f)) => self.on_frame(f, t_cap),
            Ok(None) => {
                self.diag_repeat += 1;
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
    /// display faster than the stream, the source-cadence estimate, the phase-locked hold, and
    /// the park re-arm.
    fn on_frame(&mut self, f: crate::capture::CapturedFrame, t_cap: std::time::Instant) {
        // Only a real SOURCE frame is evidence of source progress: a cursor-only
        // regeneration re-encodes the previous desktop image at a new pointer
        // position — encoded and sent like any frame, but never fed to the cadence
        // estimate, new-frame diagnostics, or capture-rebuild reset (a regenerated
        // cursor over one stashed texture is how a dead path used to look healthy).
        let source = f.provenance.origin == pf_frame::FrameOrigin::Source;
        self.frame = f;
        #[cfg(target_os = "linux")]
        if source && let Some(s) = self.restamp.as_mut() {
            self.frame.pts_ns = s.apply(self.frame.pts_ns);
        }
        if source {
            self.diag_new += 1;
        } else {
            self.diag_regen += 1;
        }
        // Source-cadence estimate: `t_cap` on the frame-driven path is taken right after
        // `wait_arrival` wakes, so real-frame deltas track the game's actual delivery spacing.
        // Deltas past 8×interval are a gap/hitch (mid-rebuild, alt-tab), not cadence — skipped.
        if source {
            if let Some(prev) = self.last_real_cap {
                let d = t_cap.duration_since(prev).as_nanos() as u64;
                if d <= self.interval.as_nanos() as u64 * 8 {
                    self.src_period_ns = Some(match self.src_period_ns {
                        Some(e) => (e as i64 + (d as i64 - e as i64) / 8) as u64,
                        None => d,
                    });
                }
            }
            self.last_real_cap = Some(t_cap);
        }
        // Phase-locked capture: hold the fresh frame so its ARRIVAL at the client lands a
        // constant small lead before the client's display latch (§3 hold-then-submit; the
        // capture slot is newest-wins, so a long hold samples fresher content next tick,
        // never staler). Adjusted ~1 Hz from the client's PhaseReports; 0 until a report
        // arrives or when PUNKTFUNK_PHASE_LOCK=0.
        if phase_lock_enabled() {
            let interval_ns = self.interval.as_nanos() as i64;
            if self.phase_ctl.due() {
                if let Some(r) = self.phase.take() {
                    self.phase_ctl.adjust(&r, interval_ns);
                } else {
                    self.phase_ctl.last_adjust = std::time::Instant::now();
                }
                self.phase.set_applied(self.phase_ctl.applied_readout());
            }
            if let Some(t) = self
                .phase_ctl
                .next_submit_target(std::time::Instant::now(), interval_ns)
            {
                let now = std::time::Instant::now();
                if t > now {
                    std::thread::sleep(t.duration_since(now));
                }
            }
        }
        if source {
            self.capture_rebuilds = 0; // a delivered SOURCE frame clears the loss counter
        }
        // Re-arm the park schedule for a (re)built display: pin the seat pointer to the
        // streamed surface (see `park_seat_pointer`). Not gamescope — its nested seat owns
        // the pointer and its cursor comes from the XFixes source regardless of seat position.
        #[cfg(target_os = "linux")]
        if self.compositor != pf_vdisplay::Compositor::Gamescope
            && self.parked_display != Some((self.cur_node_id, self.cur_display_gen))
        {
            self.parked_display = Some((self.cur_node_id, self.cur_display_gen));
            self.park_attempts = 0;
            self.next_park_at = std::time::Instant::now();
        }
    }

    /// Every 2 s under `PUNKTFUNK_PERF`: the new/repeat/regen split and the stage percentiles.
    pub(super) fn log_diag(&mut self) {
        if !(self.perf && self.diag_at.elapsed() >= std::time::Duration::from_secs(2)) {
            return;
        }
        let secs = self.diag_at.elapsed().as_secs_f64();
        tracing::info!(
            new_fps = format!("{:.0}", self.diag_new as f64 / secs),
            repeat_fps = format!("{:.0}", self.diag_repeat as f64 / secs),
            regen_fps = format!("{:.0}", self.diag_regen as f64 / secs),
            "capture diag: NEW frames from the source vs REPEATS vs cursor REGENS (low new_fps \
             at high send rate ⇒ the source isn't producing frames, not an encode stall; \
             regens alone are a cursor over a frozen image)"
        );
        let wait_max = self.st_wait.iter().copied().max().unwrap_or(0);
        tracing::info!(
            queue_us_p50 = percentile(&mut self.st_queue, 0.50),
            queue_us_p99 = percentile(&mut self.st_queue, 0.99),
            cap_us_p50 = percentile(&mut self.st_cap, 0.50),
            cap_us_p99 = percentile(&mut self.st_cap, 0.99),
            submit_us_p50 = percentile(&mut self.st_submit, 0.50),
            submit_us_p99 = percentile(&mut self.st_submit, 0.99),
            wait_us_p50 = percentile(&mut self.st_wait, 0.50),
            wait_us_p99 = percentile(&mut self.st_wait, 0.99),
            wait_us_max = wait_max,
            "stage perf (µs/call): queue=delivery→submit cap=try_latest(ring+convert) submit=encode_picture wait=lock_bitstream(sched+ASIC)"
        );
        self.st_cap.clear();
        self.st_submit.clear();
        self.st_wait.clear();
        self.st_queue.clear();
        self.diag_new = 0;
        self.diag_repeat = 0;
        self.diag_regen = 0;
        self.diag_at = std::time::Instant::now();
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
            .capturer
            .hdr_meta()
            .map(|m| self.client_hdr.unwrap_or(m));
        self.enc.set_hdr_meta(hdr_meta);
        let mut resend_meta = hdr_meta != self.last_hdr_meta;
        if resend_meta {
            self.last_hdr_meta = hdr_meta;
        }
        let max_depth = self.capturer.pipeline_depth().max(1);
        let depth = if idd_adaptive_enabled() {
            self.cur_depth.clamp(1, max_depth)
        } else {
            max_depth
        };
        let submit_ns = now_ns();
        // SyntheticCapturer counts from 0, not the epoch — fall back to "now".
        let age_ns = submit_ns.saturating_sub(self.frame.pts_ns);
        let plausible =
            self.frame.pts_ns > 0 && self.frame.pts_ns <= submit_ns && age_ns < 10_000_000_000;
        let (capture_ns, queue_us) = if !repeat && plausible {
            (self.frame.pts_ns, (age_ns / 1000) as u32)
        } else {
            (submit_ns, 0)
        };
        if self.perf && !repeat {
            self.st_queue.push(queue_us);
        }
        let t_submit = std::time::Instant::now();
        let wire_index = self.au_seq.wrapping_add(self.inflight.len() as u32);
        // An encoder the loop does not feed (the Windows driver) already holds `owed` access
        // units — waited for after a fresh frame, never on a repeat — and drains them at
        // depth 1; every other backend takes this tick's frame.
        let au_wait = if repeat {
            t_submit
        } else {
            t_submit + self.interval
        };
        let owed = self.enc.ready_aus(au_wait);
        let depth = if owed.is_some() { 1 } else { depth };
        let submitted = match owed {
            Some(_) => Ok(()),
            None => self.enc.submit_indexed(&self.frame, wire_index),
        };
        if let Err(e) = submitted {
            if e.downcast_ref::<crate::encode::TerminalEncoderError>()
                .is_some()
            {
                tracing::error!(
                    error = %format!("{e:#}"),
                    "encoder failed with a deterministic configuration error — ending the video \
                     session without rebuild attempts (see the error for the remedy)");
                return Err(e).context("encoder submit");
            }
            let Some(backoff) = self.watchdog.recover(self.interval, || {
                reset_stalled_encoder(&mut self.enc, &mut self.inflight)
            }) else {
                tracing::error!(
                    error = %format!("{e:#}"),
                    resets = self.watchdog.resets(),
                    "encoder did not recover after repeated in-place rebuilds — ending the video \
                     session (see the error above for the cause)");
                return Err(e).context("encoder submit");
            };
            tracing::warn!(error = %format!("{e:#}"), reset = self.watchdog.resets(),
                max = MAX_ENCODER_RESETS,
                "encoder submit failed — encoder rebuilt in place, forcing an IDR");
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
        if self.perf {
            self.st_submit.push(submit_us);
        }
        self.next = if frame_driven_enabled() && self.capturer.supports_arrival_wait() {
            if !repeat {
                self.pace.charge();
            }
            std::time::Instant::now() + self.interval
        } else {
            self.next + self.interval
        };
        for _ in 0..owed.unwrap_or(1) {
            self.inflight.push_back((capture_ns, submit_ns, self.next));
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
        while self.inflight.len() >= depth {
            if self.streamed_wire && self.enc.supports_chunked_poll() {
                match self.poll_chunked(&stamps, &mut resend_meta) {
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
            match self.poll_whole(&stamps, &mut resend_meta) {
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
            // The send thread ended. With the loop stopping or the link closed that is the
            // session ending; otherwise it died on an error it logged, which is not a clean end.
            if !self.stop.load(Ordering::SeqCst) && self.conn.close_reason().is_none() {
                anyhow::bail!("video send thread stopped");
            }
            return Ok(Flow::Break);
        }
        self.check_encode_stall(depth, poll_err)?;
        Ok(Flow::Next)
    }

    /// Fold this poll's elapsed time into the tick's longest submit+poll chain —
    /// the backend-neutral pressure input `adapt_depth` reads. Driver-owned AUs
    /// carry no host submit, so they never count.
    fn note_encode_chain(&mut self, st: &Stamps, t_wait: std::time::Instant) {
        if st.owed {
            return;
        }
        self.encode_chain_ns = self.encode_chain_ns.max(
            st.submit_elapsed
                .saturating_add(t_wait.elapsed())
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64,
        );
    }

    /// The AU-level fields for the AU just taken, from its `inflight` stamps
    /// `(capture, submit, deadline)`. On the driver the host submits nothing: the stages are
    /// the driver's own, and its per-AU present time (`pts_ns`) beats the tick's clock, which
    /// would give every AU of a burst the same one.
    fn au_meta(
        &self,
        st: &Stamps,
        (cap_ns, sub_ns, deadline): (u64, u64, std::time::Instant),
        pts_ns: u64,
        flags: u32,
        wait_us: u32,
    ) -> AuMeta {
        let d = if st.owed {
            driver_stages(&*self.enc)
        } else {
            AuStages::host((now_ns().saturating_sub(sub_ns) / 1000) as u32, st.queue_us)
        };
        AuMeta {
            capture_ns: if st.owed && pts_ns > 0 {
                pts_ns
            } else {
                cap_ns
            },
            epoch: self.epoch,
            flags,
            frame_index: self.au_seq,
            deadline,
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

    /// Stream one AU's chunks as they land. `Au` = a whole AU went out (the caller polls again
    /// while owed frames remain); `Nothing` = the encoder has no more output this tick.
    fn poll_chunked(&mut self, st: &Stamps, resend_meta: &mut bool) -> Polled {
        let t_wait = std::time::Instant::now();
        let mut first_chunk_us = 0u32;
        let mut flags = 0u32;
        loop {
            let polled = self.enc.poll_chunk();
            self.note_encode_chain(st, t_wait);
            let c = match polled {
                Ok(Some(c)) => c,
                Ok(None) => return Polled::Nothing,
                Err(e) => return Polled::Failed(e),
            };
            self.watchdog.on_au();
            // A FIRST while the previous AU's LAST never came (the driver dropped its
            // tail, or a reset abandoned it): close that frame on the wire first, or
            // this AU is spliced onto a truncated prefix under its own flags.
            if c.first && self.wire_frame_open {
                if self.inflight.len() > 1 {
                    self.inflight.pop_front();
                }
                self.au_seq = self.au_seq.wrapping_add(1);
            }
            self.wire_frame_open = !c.last;
            if c.first {
                first_chunk_us = t_wait.elapsed().as_micros() as u32;
                let caps = self.enc.caps();
                flags = au_flags(
                    &caps,
                    &mut self.ir_wave_pos,
                    c.keyframe,
                    c.recovery_point,
                    c.recovery_close,
                    c.recovery_anchor,
                    c.chunk_aligned,
                    &self.counters.link,
                );
                self.send_hdr_meta(c.keyframe, resend_meta);
                self.bringup.mark("first_au");
            }
            let last = c.last;
            let inflight = *self.inflight.front().expect("inflight non-empty");
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
            if self.frame_tx.send(SendMsg::Chunk(msg)).is_err() {
                return Polled::SendGone;
            }
            if last {
                self.inflight.pop_front();
                self.au_seq = self.au_seq.wrapping_add(1);
                self.sent += 1;
                if self.perf {
                    self.st_wait.push(wait_total_us);
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
    fn poll_whole(&mut self, st: &Stamps, resend_meta: &mut bool) -> Polled {
        let t_wait = std::time::Instant::now();
        let polled = self.enc.poll();
        let wait_elapsed = t_wait.elapsed();
        self.note_encode_chain(st, t_wait);
        let wait_us = if st.measure {
            wait_elapsed.as_micros() as u32
        } else {
            0
        };
        if self.perf {
            self.st_wait.push(wait_us);
        }
        let au = match polled {
            Ok(Some(au)) => au,
            Ok(None) => return Polled::Nothing,
            Err(e) => return Polled::Failed(e),
        };
        self.watchdog.on_au();
        let inflight = self.inflight.pop_front().expect("inflight non-empty");
        let caps = self.enc.caps();
        let flags = au_flags(
            &caps,
            &mut self.ir_wave_pos,
            au.keyframe,
            au.recovery_point,
            au.recovery_close,
            au.recovery_anchor,
            au.chunk_aligned,
            &self.counters.link,
        );
        self.send_hdr_meta(au.keyframe, resend_meta);
        let msg = FrameMsg {
            meta: self.au_meta(st, inflight, au.pts_ns, flags, wait_us),
            data: au.data,
        };
        self.bringup.mark("first_au");
        if self.frame_tx.send(SendMsg::Frame(msg)).is_err() {
            return Polled::SendGone;
        }
        self.au_seq = self.au_seq.wrapping_add(1);
        self.sent += 1;
        Polled::Au
    }

    /// HDR metadata rides a datagram on every keyframe and once when it changes.
    fn send_hdr_meta(&self, keyframe: bool, resend_meta: &mut bool) {
        let Some(m) = self.last_hdr_meta else {
            return;
        };
        if keyframe || *resend_meta {
            let _ = self
                .conn
                .send_datagram(punktfunk_core::quic::encode_hdr_meta_datagram(
                    &crate::encode::hdr_meta_to_wire(m),
                ));
            *resend_meta = false;
        }
    }

    /// A poll failure, or owed frames with no AU for the stall window: rebuild the encoder in
    /// place, up to [`MAX_ENCODER_RESETS`] times.
    fn check_encode_stall(&mut self, depth: usize, poll_err: Option<anyhow::Error>) -> Result<()> {
        let stalled = self
            .watchdog
            .stalled(self.inflight.len(), depth, self.interval);
        let why = match (&poll_err, stalled) {
            (Some(e), _) => format!("poll failed: {e:#}"),
            (None, Some(why)) => why,
            (None, None) => return Ok(()),
        };
        let recovered = self.watchdog.recover(self.interval, || {
            reset_stalled_encoder(&mut self.enc, &mut self.inflight)
        });
        if recovered.is_none() {
            return Err(poll_err.unwrap_or_else(|| anyhow!("{why}")))
                .context("encoder stalled — in-place rebuild unavailable or exhausted");
        }
        tracing::warn!(reset = self.watchdog.resets(), max = MAX_ENCODER_RESETS, %why,
            "encode stall detected — encoder rebuilt in place, forcing an IDR");
        Ok(())
    }

    /// IDD pipeline depth / pipelined-retrieve escalation on a sustained behind-cadence score,
    /// wound back after a clean run with a growing backoff. Also publishes the cadence verdict
    /// the ABR climb gate reads.
    pub(super) fn adapt_depth(&mut self) {
        if !idd_adaptive_enabled() {
            return;
        }
        self.depth_frames += 1;
        if self.depth_frames <= DEPTH_WARMUP_FRAMES {
            return;
        }
        let max_depth = self.capturer.pipeline_depth().max(1);
        let budget = cadence_budget(self.interval, self.src_period_ns);
        let schedule_behind = std::time::Instant::now() >= self.next + (budget - self.interval);
        let encode_pressure = encode_chain_pressure(budget, self.encode_chain_ns);
        let behind = schedule_behind || encode_pressure;
        self.behind_score = if behind {
            (self.behind_score + 1).min(DEPTH_BEHIND_CAP)
        } else {
            self.behind_score.saturating_sub(1)
        };
        let escalated = self.cur_depth > 1 || self.pipelined_active || self.deescalating;
        let degraded = encode_behind_cadence(escalated, self.behind_score, DEPTH_DEGRADE);
        self.cadence_degraded.store(degraded, Ordering::Relaxed);
        self.cadence_behind_score
            .store(self.behind_score, Ordering::Relaxed);
        if degraded != self.was_degraded {
            let now = std::time::Instant::now();
            if self
                .last_cadence_log
                .is_none_or(|t| now.duration_since(t) >= CADENCE_LOG_MIN_GAP)
            {
                if degraded {
                    tracing::info!(
                        behind_score = self.behind_score,
                        escalated,
                        encode_pressure,
                        budget_us = budget.as_micros() as u64,
                        interval_us = self.interval.as_micros() as u64,
                        src_period_us = self.src_period_ns.map(|p| p / 1_000).unwrap_or_default(),
                        flips_suppressed = self.cadence_flips_suppressed,
                        "encode behind cadence — ABR climbs will be refused until it recovers"
                    );
                    // Name any other encoder on the engine; Windows pauses Instant Replay there.
                    crate::encoder_sessions::on_behind_cadence();
                } else {
                    tracing::info!(
                        behind_score = self.behind_score,
                        encode_pressure,
                        flips_suppressed = self.cadence_flips_suppressed,
                        "encode cadence recovered — ABR climbs allowed again"
                    );
                }
                self.last_cadence_log = Some(now);
                self.cadence_flips_suppressed = 0;
            } else {
                self.cadence_flips_suppressed += 1;
            }
            self.was_degraded = degraded;
        }
        if self.deescalating {
            if !self.enc.set_pipelined(false) {
                self.deescalating = false;
                self.pipelined_active = false;
                self.pipeline_asked = false;
                tracing::info!(
                    "encoder pipelined retrieve de-escalated — sync retrieve (and \
                     sub-frame streaming, where armed) restored; re-monitoring cadence"
                );
                self.behind_score = 0;
                self.depth_frames = 0;
                self.ahead_run = 0;
            }
        } else if self.behind_score >= DEPTH_ESCALATE
            && (self.cur_depth < max_depth || !self.pipeline_asked)
        {
            if self.cur_depth < max_depth {
                self.cur_depth = max_depth;
                tracing::info!(
                    depth = self.cur_depth,
                    "IDD pipeline depth escalated — encode can't hold cadence at depth-1 \
                     (GPU contention); pipelining until cadence holds clean (latency \
                     trade for throughput)"
                );
            } else {
                self.pipeline_asked = true;
                self.pipelined_active = self.enc.set_pipelined(true);
                if self.pipelined_active {
                    tracing::info!(
                        "encoder pipelined retrieve escalated — encode can't hold \
                         cadence and the capturer has no depth to give; the encode wait \
                         moves off the loop until cadence holds clean (latency trade \
                         for throughput)"
                    );
                }
            }
            self.behind_score = 0;
            self.ahead_run = 0;
        } else if escalated {
            self.ahead_run = if behind { 0 } else { self.ahead_run + 1 };
            if self.ahead_run >= DEESCALATE_CLEAN_FRAMES
                && self
                    .deescalate_not_before
                    .is_none_or(|t| std::time::Instant::now() >= t)
            {
                self.ahead_run = 0;
                self.deescalate_not_before =
                    Some(std::time::Instant::now() + self.deescalate_backoff);
                self.deescalate_backoff = (self.deescalate_backoff * 5).min(DEESCALATE_BACKOFF_MAX);
                if self.pipelined_active {
                    tracing::info!(
                        "cadence held clean while escalated — winding the pipelined \
                         retrieve back (latency recovery; costs one IDR)"
                    );
                    self.deescalating = true;
                } else if self.cur_depth > 1 {
                    self.cur_depth = 1;
                    tracing::info!(
                        depth = self.cur_depth,
                        "IDD pipeline depth de-escalated — cadence held clean at the \
                         escalated depth (latency recovery)"
                    );
                    self.behind_score = 0;
                    self.depth_frames = 0;
                }
            }
        }
    }

    /// Wait for the next tick: the capturer's arrival wait under the credit pacer, the next
    /// access unit of an encoder that publishes its own, or the fixed-interval sleep. A
    /// repeat holds no slot: the real frame after it goes out on arrival.
    pub(super) fn sleep_to_next(&mut self, t_cap: std::time::Instant, repeat: bool) {
        if !frame_driven_enabled() {
            return self.sleep_to_grid();
        }
        if self.capturer.supports_arrival_wait() {
            // Anchor the 0.9× floor to `t_cap`, not `next`: a sync encoder folds encode into cadence.
            if !repeat {
                let earliest = std::cmp::max(
                    t_cap + self.interval.mul_f32(0.9),
                    self.pace.earliest(std::time::Instant::now(), self.interval),
                );
                if let Some(d) = earliest.checked_duration_since(std::time::Instant::now()) {
                    std::thread::sleep(d);
                }
            }
            self.capturer
                .wait_arrival(self.next + keepalive_wait(self.interval, self.src_period_ns));
        } else if self.enc.ready_aus(self.next).is_some() {
            // An access unit landed or the period ran out. The grid restarts here: on its own
            // phase it holds a finished AU for up to a period, a different one every session.
            self.next = std::time::Instant::now();
        } else {
            self.sleep_to_grid();
        }
    }

    /// The fixed-cadence tick, re-anchored when the loop is behind it.
    fn sleep_to_grid(&mut self) {
        match self.next.checked_duration_since(std::time::Instant::now()) {
            Some(d) => std::thread::sleep(d),
            None => self.next = std::time::Instant::now(),
        }
    }

    /// After the loop: poll what the encoder still owes into the send thread.
    /// Pipelined retrieve must be wound back first — a probing `poll` would
    /// return `None` on the first in-flight AU and strand the tail.
    pub(super) fn drain(&mut self) {
        self.enc.set_pipelined(false);
        while let Some((cap_ns, sub_ns, deadline)) = self.inflight.pop_front() {
            let Ok(Some(au)) = self.enc.poll() else { break };
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
                    epoch: self.epoch,
                    flags,
                    frame_index: self.au_seq,
                    deadline,
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
            if self.frame_tx.send(SendMsg::Frame(msg)).is_err() {
                break;
            }
            self.au_seq = self.au_seq.wrapping_add(1);
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
    use super::*;

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
        assert!(!encode_chain_pressure(ms(10), 3_000_000));
        assert!(encode_chain_pressure(ms(10), 8_000_000));
        assert!(encode_chain_pressure(ms(10), 13_000_000));
        assert!(!encode_chain_pressure(
            ms(16) + std::time::Duration::from_micros(667),
            13_000_000
        ));
        assert!(encode_chain_pressure(ms(13), 13_000_000));
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

#[cfg(test)]
mod keepalive_tests {
    use super::keepalive_wait;
    use std::time::Duration;

    /// A source at 2× the interval gets its next frame before any repeat; one at the
    /// interval, or none measured, keeps the plain half-interval keep-alive.
    #[test]
    fn a_slower_steady_source_outlives_the_keepalive() {
        let i = Duration::from_micros(8_333);
        assert_eq!(keepalive_wait(i, None), i.mul_f32(0.5));
        assert_eq!(keepalive_wait(i, Some(i.as_nanos() as u64)), i.mul_f32(0.5));
        let two = keepalive_wait(i, Some(2 * i.as_nanos() as u64));
        assert!(two > i.mul_f32(1.4) && two < i.mul_f32(1.6), "{two:?}");
    }
}
