//! Video pacing: frame intake, the latch clock, the glass gate, and the 1 Hz window.

use super::pace_step::{PaceAction, PaceFacts};
use super::*;

impl Shell {
    /// This pass's video: HDR metadata, the glass fold, intake, the pace step and its
    /// action, and the 1 Hz window. `true` when a frame reached the swapchain, carrying
    /// the overlay with it.
    pub(super) fn video_tick(&mut self, st: &mut StreamState) -> Result<bool> {
        // Mastering metadata (0xCE) → the presentation engine, ahead of the frame
        // that needs it.
        if let Some(c) = &st.connector {
            while let Ok(m) = c.next_hdr_meta(Duration::ZERO) {
                self.presenter.set_hdr_metadata(m);
            }
        }
        // Present-wait completions drive the latch clock, the glass gate, and the
        // host-facing grid — drained every pass (a 1 Hz batch would starve all three).
        // Without a glass clock too: the compositor queues a feedback answer per present.
        let samples = self.presenter.take_presented_samples();
        if !samples.is_empty() {
            st.fold_glass(
                &samples,
                self.presenter.vblank_locked(),
                self.presenter.glass_estimated(),
            );
        }
        st.intake();
        self.presenter.begin_present_turn(st.newest_turn);
        st.win.ticks += 1;
        if let Some(refresh_ns) = self.presenter.measured_refresh_ns() {
            if self.presenter.vblank_locked() {
                st.cadence.note_refresh(refresh_ns, st.mode_period_ns);
            }
        }
        if let Some((cycle_ns, interval_ns)) = self.presenter.engine_refresh() {
            st.note_engine_refresh(cycle_ns, interval_ns);
        }
        let now_ns = session::now_ns();
        let facts = st.pace_facts(&self.presenter, self.pacing_active, now_ns);
        let slot_after = |t: u64| st.clock.next_slot_after(t);
        // Newest-wins at a latch slot keeps one frame of backlog; everywhere else the
        // store's own rule.
        let due = move |p: &Paced| p.due_ns <= now_ns as i64;
        let take = || {
            if facts.on_grid() && !facts.smoothing {
                st.store.take_due_bounded(due)
            } else {
                st.store.take(due)
            }
        };
        let action = st
            .pace
            .step(&mut st.slots, &mut st.gate, &facts, slot_after, take);
        let presented = match action {
            PaceAction::Vend { frame, aim_ns } => self.present_vended(st, frame, aim_ns, now_ns)?,
            PaceAction::Park(frame) => {
                st.store.put_back(frame);
                false
            }
            // The loop's sleep is `wake_timeout`: the same rule over the store as it stands.
            PaceAction::Wait => false,
        };
        // Nothing taken in is waiting on this pass any more, unless the presenter was busy.
        if !st.pace.is_busy() {
            self.presenter.end_present_turn(st.newest_turn);
        }
        // Close the overlay window once per second.
        if st.win.start.elapsed() >= Duration::from_secs(1) {
            self.close_present_window(st);
        }
        Ok(presented)
    }

    /// Present the frame the pace step vended at `aim_ns`, then book it: the HUD window,
    /// the ready line, and under a glass clock the gate and the aim its stamp is checked
    /// against. `true` when it reached the swapchain.
    fn present_vended(
        &mut self,
        st: &mut StreamState,
        paced: Paced,
        aim_ns: u64,
        now_ns: u64,
    ) -> Result<bool> {
        let (pts_ns, decoded_ns) = (paced.frame.pts_ns, paced.frame.decoded_ns);
        tracing::trace!(
            pts_ns,
            // Present pass against the due time (0 = none); against arrival otherwise.
            slip_us = (now_ns as i64 - paced.due_ns) / 1000 * i64::from(paced.due_ns != 0),
            waited_us = now_ns.saturating_sub(decoded_ns) / 1000,
            "frame out"
        );
        // Resize end: a frame at the steered target size means the new-mode
        // picture is here.
        let (fw, fh) = paced.frame.image.dimensions();
        st.resize_overlay.decoded(fw, fh);
        st.last_video = Some((fw, fh));
        let overlay = self.overlay_frame.as_ref();
        if !st.present(&mut self.presenter, &self.window, overlay, paced)? {
            return Ok(false);
        }
        self.overlay_damage.video_presented(Instant::now());
        st.win.push(&self.presenter);
        if self.opts.json_status && !st.ready_announced {
            st.ready_announced = true;
            emit(SessionLine::Ready);
        }
        if self.presenter.present_timing_active() {
            // Hand the frame's stamps to the present-wait waiter — e2e/display
            // samples arrive via `take_presented_samples` with a true on-glass stamp.
            self.presenter.note_presented(pts_ns, decoded_ns);
            st.gate.note_present(now_ns);
            st.win.out_max = st.win.out_max.max(self.presenter.presents_outstanding());
            st.aimed.push_back((pts_ns, aim_ns));
            while st.aimed.len() > 16 {
                st.aimed.pop_front();
            }
        } else {
            st.note_submitted(pts_ns, decoded_ns);
        }
        Ok(true)
    }

    /// The 1 Hz close: the HUD window, the adaptive slot margin, and the presenter line.
    fn close_present_window(&mut self, st: &mut StreamState) {
        let [import, submit, fence, acquire, queue_present] = st.win.take_timings();
        // Drained once per window and shared by the HUD and the log line — a
        // second `take_counters` would read zeros.
        let (replaced, q_drop) = st.store.take_counters();
        let (gated, forced) = st.gate.take_counters();
        let forwarded = st.forwarder_drops.swap(0, Ordering::Relaxed);
        let cadence_err = hud::Summary::of(&mut st.win.cadence_err_us);
        st.win.cadence_err_us.clear();
        let grid_err = hud::Summary::of(&mut st.win.grid_err_us);
        st.win.grid_err_us.clear();
        let unshown = self.presenter.take_unshown();
        st.last_forced = forced;
        // Drained once: the native lane's count where it showed anything, else the
        // compositor's word on the swapchain.
        #[cfg(target_os = "linux")]
        let native_zero_copy = self.presenter.take_native_zero_copy();
        #[cfg(not(target_os = "linux"))]
        let native_zero_copy = (0u32, 0u32);
        #[cfg(target_os = "linux")]
        let scanout = (native_zero_copy.1 > 0)
            .then_some(native_zero_copy)
            .or_else(|| self.presenter.take_scanout());
        #[cfg(not(target_os = "linux"))]
        let scanout = None;
        let present = PresentCounters {
            mode: self.presenter.present_mode_name(),
            vrr: st.cadence.verdict(),
            smoothing: st.store.is_smoothing(),
            q_drop,
            gated,
            forced,
            forwarded,
            scanout,
        };
        let (pace_ms, latch_ms) = close_window(
            st,
            &self.presenter,
            &present,
            replaced,
            self.stats_verbosity,
        );
        // Submit → our GPU done (latch) and GPU done → glass (floor), p50 and the window's
        // worst: a hold after the present call lands in one of the two.
        let ms = |us: u32| us as f32 / 1000.0;
        let (latch_max_ms, floor_ms, floor_max_ms) =
            st.last_snap.as_ref().map_or((0.0, 0.0, 0.0), |s| {
                (
                    ms(s.latch.max_us),
                    ms(s.os_floor.p50_us),
                    ms(s.os_floor.max_us),
                )
            });
        st.win.start = Instant::now();
        // Under MAILBOX a missed latch leaves no stamp: the successor replaces the frame and
        // the engine reports it unshown. Both feed the margin, for either intent on a grid.
        let on_grid =
            (st.store.is_smoothing() || st.pacer.paces_latency()) && !st.pacer.free_running();
        // From a settled window only: a stream still ramping, or arrival jitter past an
        // eighth of a period, misses latches for reasons of its own and would widen the
        // margin to its cap in the first seconds of every session.
        let shown: u64 = st.win.steps.iter().map(|&n| u64::from(n)).sum();
        let settled = shown.saturating_mul(st.source_interval_ns.max(0) as u64) >= 800_000_000
            && st.pacer.health().jitter_ns < st.clock.period_ns() as i64 / 8;
        if on_grid && settled && st.slots.note_window(st.win.misses + unshown, st.win.early) {
            tracing::info!(
                margin_us = st.slots.margin_ns() / 1000,
                misses = st.win.misses,
                unshown,
                "slot margin widened (missed or replaced latches)"
            );
        }
        // The 1 Hz presenter line, always: the field bundle's only record of where a
        // frame went after decode and how evenly the glass stepped.
        if self.pacing_active {
            let cadence_health = st.pacer.health();
            let shown: u32 = st.win.steps.iter().sum();
            let mode_count = st.win.steps.iter().copied().max().unwrap_or(0);
            // Spacings off the most common step, per mille of the window's presents.
            let judder = if shown > 0 {
                u64::from(shown - mode_count) * 1000 / u64::from(shown)
            } else {
                0
            };
            tracing::info!(
                smoothing = present.smoothing,
                // The latency intent held for its due time or its slot: a measured panel.
                paced = st.pacer.paces_latency(),
                mode = present.mode,
                // Where the display stamps came from; `none` means the glass fields
                // below (steps, judder, misses, gated) measured nothing.
                glass = self.presenter.glass_source(),
                vrr = present.vrr.label(),
                native_zero_copy = ?native_zero_copy,
                scanout = ?present.scanout,
                replaced,
                q_drop,
                forwarded = present.forwarded,
                repeats = st.win.repeats,
                // Loop passes this second: far above the frame rate is a spin.
                ticks = st.win.ticks,
                // On-glass spacing error against the source's spacing, per shown frame.
                cadence_err_us = cadence_err.p50_us,
                cadence_err_p95_us = cadence_err.p95_us,
                // Stamps the engine took itself, and how far the on-glass spacing sat
                // from a whole number of refreshes: a fixed panel's is stamp noise.
                exact = st.win.exact,
                grid_err_us = grid_err.p50_us,
                grid_err_p95_us = grid_err.p95_us,
                // Presents the engine never showed: replaced before a refresh took them.
                unshown,
                gated,
                forced,
                misses = st.win.misses,
                // Frames shown before the latch they were aimed at: the lead is long.
                early = st.win.early,
                out_max = st.win.out_max,
                steps = ?st.win.steps,
                busy = ?st.win.busy,
                judder,
                pace_ms,
                latch_ms,
                latch_max_ms,
                floor_ms,
                floor_max_ms,
                import_us = import.p50_us,
                submit_us = submit.p50_us,
                submit_max_us = submit.max_us,
                fence_us = fence.p50_us,
                fence_max_us = fence.max_us,
                acquire_us = acquire.p50_us,
                acquire_max_us = acquire.max_us,
                present_us = queue_present.p50_us,
                period_us = st.clock.period_ns() / 1000,
                // The output's own vblank spacing where a waiter measures it; 0 elsewhere.
                refresh_us = self.presenter.measured_refresh_ns().unwrap_or(0) / 1000,
                margin_us = st.slots.margin_ns() / 1000,
                // Submit lead ahead of the aimed latch (smoothing on a grid).
                lead_us = st.slots.lead_ns(st.clock.period_ns()) / 1000,
                // Cadence loop's current hold and the jitter it is sized from,
                // plus frames whose due time had already passed when they arrived.
                // Cumulative/instantaneous, not window sums like the counters above.
                cushion_us = cadence_health.cushion_ns / 1000,
                jitter_us = cadence_health.jitter_ns / 1000,
                tail_us = cadence_health.tail_ns / 1000,
                late = cadence_health.late,
                "presenter window"
            );
        }
        st.win.reset_counts();
    }
}

impl StreamState {
    /// Fold present-wait completions into the latch clock, the VRR probe, and this window's
    /// misses and steps. `vblank_locked`: the present mode waits for vblank, which the VRR
    /// probe needs. `estimated`: the stamps are the vblank waiter's estimate, which never
    /// trains the latch clock — on a variable panel those vblanks follow our own presents.
    pub(super) fn fold_glass(
        &mut self,
        samples: &[crate::vk::PresentedSample],
        vblank_locked: bool,
        estimated: bool,
    ) {
        let clock_offset_ns = self.clock_offset_ns();
        let period = self.clock.period_ns();
        let mut stamps = Vec::with_capacity(samples.len());
        let mut glass = Vec::with_capacity(samples.len());
        for s in samples {
            self.win.exact += u32::from(s.exact);
            self.cadence.note_source(s.pts_ns);
            if self.last_displayed_ns != 0 && s.displayed_ns > self.last_displayed_ns {
                let off = off_grid_ns(s.displayed_ns - self.last_displayed_ns, self.mode_period_ns);
                self.win
                    .grid_err_us
                    .push((off / 1000).min(u64::from(u32::MAX)) as u32);
            }
            // Hand the audio plane the figure it has to hit: the on-glass branch.
            self.publish_e2e(clock_offset_ns, s.displayed_ns, s.pts_ns);
            if let Some(c) = &self.connector {
                c.hud().note_displayed_split(
                    s.pts_ns,
                    s.decoded_ns,
                    s.submitted_ns,
                    s.gpu_done_ns,
                    s.displayed_ns,
                );
            }
            // Latch miss: the frame landed past the slot it was aimed at (half a period
            // of stamp slack). Smoothing without an aim: past one period plus the lead from
            // submit. Store evictions happen whenever the stream out-runs the panel and say
            // nothing about the latch.
            let target_ns = self
                .aimed
                .iter()
                .position(|&(pts, _)| pts == s.pts_ns)
                .and_then(|i| self.aimed.remove(i))
                .map_or(0, |(_, t)| t);
            if target_ns > 0 || self.store.is_smoothing() {
                let missed = if target_ns > 0 {
                    s.displayed_ns > target_ns + period / 2
                } else {
                    s.displayed_ns.saturating_sub(s.submitted_ns) > period + self.slots.margin_ns()
                };
                self.win.misses += u32::from(missed);
                // A slot before its aim: the lead reaches past the previous latch.
                self.win.early +=
                    u32::from(target_ns > 0 && s.displayed_ns + period / 2 < target_ns);
            }
            tracing::trace!(
                pts_ns = s.pts_ns,
                displayed_ns = s.displayed_ns,
                target_ns,
                submitted_ns = s.submitted_ns,
                "frame glass"
            );
            if self.last_displayed_ns != 0 && period > 0 {
                let steps =
                    (s.displayed_ns.saturating_sub(self.last_displayed_ns) + period / 2) / period;
                self.win.steps[(steps as usize).min(5)] += 1;
            }
            // On-glass spacing against the source's own: valid on a fixed panel and a
            // variable one alike, where whole-period steps mean nothing.
            if self.last_displayed_ns != 0 && s.pts_ns > self.last_shown_pts_ns {
                let glass = s.displayed_ns.saturating_sub(self.last_displayed_ns);
                let source = s.pts_ns - self.last_shown_pts_ns;
                let err_us = glass.abs_diff(source) / 1000;
                self.win
                    .cadence_err_us
                    .push(err_us.min(u64::from(u32::MAX)) as u32);
            }
            self.last_shown_pts_ns = s.pts_ns;
            self.last_displayed_ns = s.displayed_ns;
            stamps.push(s.displayed_ns);
            // An exact stamp (the engine's or the compositor's) is a display time in any
            // mode; a wake time is one only where the wait ends on a vblank.
            if s.exact || vblank_locked {
                glass.push((s.displayed_ns, s.pts_ns));
            }
        }
        if !estimated {
            self.clock.note_batch(&stamps, self.store.is_smoothing());
        }
        // VRR probe: healthy-window stamps only, against the display mode's period (the
        // learned one adopts a slow stream's cadence as "the grid").
        let healthy = self.last_forced == 0;
        self.cadence.note(&glass, self.mode_period_ns, healthy);
    }

    /// Intake into the intent store. PyroWave collapses smoothness to latency: its
    /// plane-ring retirement assumes the newest-wins hand-off, and all-intra frames
    /// make buffering moot.
    pub(super) fn intake(&mut self) {
        while let Ok(f) = self.frames.try_recv() {
            let repeat = f.repeat;
            self.win.repeats += u32::from(repeat);
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            if let DecodedImage::PyroWave(p) = &f.image {
                self.newest_turn = p.turn;
            }
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            if self.store.is_smoothing() && matches!(f.image, DecodedImage::PyroWave(_)) {
                self.store.force_latency();
                if !self.pyro_latency_forced {
                    self.pyro_latency_forced = true;
                    tracing::info!(
                        "PyroWave stream — smoothness buffering does not apply \
                         (latency pacing)"
                    );
                }
            }
            // Intent after any PyroWave collapse above, so a wavelet stream folds
            // nothing into a loop it will never consult.
            let smoothing = self.store.is_smoothing();
            // A repeat is shown on arrival and stays out of the cadence clock: it is the
            // picture already on glass, and its near-free encode would read as an early
            // arrival. Shown, it keeps a VRR panel off its own refresh filler.
            let due_ns = if repeat {
                0
            } else {
                self.pacer
                    .due_ns(smoothing, f.pts_ns, f.decoded_ns, self.source_interval_ns)
                    .unwrap_or(0)
            };
            tracing::trace!(
                pts_ns = f.pts_ns,
                repeat,
                // Hold the pacer asks for; negative means the frame arrived past its due time.
                hold_us = (due_ns - f.decoded_ns as i64) / 1000 * i64::from(due_ns != 0),
                "frame in"
            );
            self.store.submit(Paced { frame: f, due_ns });
        }
    }

    /// This pass's facts for the pace step. The pacer follows the measured refresh first,
    /// so the step reads this pass's intents. An estimated stamp never builds the latch
    /// grid: on a VRR panel the vblanks it reads follow our own presents.
    fn pace_facts(&mut self, presenter: &Presenter, pacing: bool, now_ns: u64) -> PaceFacts {
        let glass = presenter.present_timing_active();
        let estimated = presenter.glass_estimated();
        let verdict = self.cadence.verdict();
        let smoothing = self.store.is_smoothing();
        self.pacer.follow(verdict, glass && !estimated, smoothing);
        self.store.set_paced(self.pacer.paces_latency());
        PaceFacts {
            now_ns,
            period_ns: self.clock.period_ns(),
            front_due_ns: self.front_due_ns(),
            outstanding: presenter.presents_outstanding(),
            pacing,
            fifo: presenter.needs_glass_gate(),
            glass,
            estimated,
            anchored: self.clock.anchored(),
            verdict,
            smoothing,
            paces_latency: self.pacer.paces_latency(),
            free_running: self.pacer.free_running(),
        }
    }

    /// When the store's front frame is due; a negative due time is due now.
    fn front_due_ns(&self) -> Option<u64> {
        self.store.front().map(|p| p.due_ns.max(0) as u64)
    }

    /// Present one paced frame through its lane. `Ok(true)`: it reached the swapchain.
    /// Only a lost device is an error; other failures run the lane's [`Rung`] policy.
    pub(super) fn present(
        &mut self,
        presenter: &mut Presenter,
        window: &sdl3::video::Window,
        overlay: Option<&OverlayFrame>,
        paced: Paced,
    ) -> Result<bool> {
        let Paced {
            frame:
                DecodedFrame {
                    pts_ns,
                    decoded_ns,
                    repeat: _,
                    image,
                },
            due_ns,
        } = paced;
        let (rung, res) = match image {
            // PyroWave: already on the presenter's device and fence-complete — a
            // present failure has no demote rung; only device loss ends the session.
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            DecodedImage::PyroWave(f) => {
                // Wavelet stream carries negotiated ColorInfo (no VUI): a PQ
                // session presents through the HDR10 path like the H.26x codecs.
                self.hdr = f.color.is_pq();
                self.hdr_untonemapped = false;
                // The native lane first: the planes copied into the window's buffer.
                let res = match presenter.present_native_pyro(f, pts_ns, decoded_ns) {
                    crate::vk::NativeVkOutcome::Shown => Ok(Presented::Shown),
                    crate::vk::NativeVkOutcome::Dropped => Ok(Presented::Stale),
                    crate::vk::NativeVkOutcome::Declined(f) => {
                        presenter.present(window, FrameInput::PyroWave(f), overlay)
                    }
                };
                (Rung::PyroWave, res.map(Shown::of))
            }
            DecodedImage::Cpu(c) => {
                self.hdr = c.color.is_pq();
                // Software lane uploads planes into the same planar CSC pass as
                // hardware, so PQ is tone-mapped there too.
                self.hdr_untonemapped = false;
                // The lane borrows its frame, and the borrow ends inside `map`, so a busy
                // frame goes back whole from here.
                let res = presenter
                    .present(window, FrameInput::Cpu(&c), overlay)
                    .map(Shown::of);
                let res = res.map(|s| match s {
                    Shown::Busy(_, on) => Shown::Busy(Some(DecodedImage::Cpu(c)), on),
                    s => s,
                });
                (Rung::Software, res)
            }
            // VAAPI output: dmabuf fds plus a plane layout. Import and failure-
            // streak demotion are the same contract as the other hardware arms.
            #[cfg(target_os = "linux")]
            DecodedImage::NativeDmabuf(d)
                if presenter.supports_dmabuf() && !self.health.demoted(Lane::Import) =>
            {
                self.hdr = d.color.is_pq();
                self.hdr_untonemapped = false;
                // The native lane first: the compositor takes the dma-buf itself.
                let res = match presenter.present_native(d, pts_ns, decoded_ns) {
                    crate::wl_native::Outcome::Shown => Ok(Presented::Shown),
                    crate::wl_native::Outcome::Dropped => Ok(Presented::Stale),
                    crate::wl_native::Outcome::Declined(d) => {
                        presenter.present(window, FrameInput::Dmabuf(d), overlay)
                    }
                };
                (Rung::Hardware(Lane::Import), res.map(Shown::of))
            }
            #[cfg(target_os = "linux")]
            DecodedImage::NativeDmabuf(_) => {
                // No import extensions (or already demoted) — the pump demotes the decoder.
                if self.health.demote(Lane::Import) {
                    tracing::warn!(
                        "no dmabuf import support on this device — demoting the decoder"
                    );
                    self.force_software.store(true, Ordering::Relaxed);
                }
                return Ok(false);
            }
            // D3D11VA: shared-texture import, same gate + failure-streak demotion
            // as dmabuf.
            #[cfg(windows)]
            DecodedImage::D3d11(d)
                if presenter.supports_d3d11() && !self.health.demoted(Lane::Import) =>
            {
                self.hdr = d.color().is_pq();
                self.hdr_untonemapped = false;
                let res = presenter.present(window, FrameInput::D3d11(d), overlay);
                (Rung::Hardware(Lane::Import), res.map(Shown::of))
            }
            #[cfg(windows)]
            DecodedImage::D3d11(_) => {
                // No import extensions (or already demoted) — the pump demotes the decoder.
                if self.health.demote(Lane::Import) {
                    tracing::warn!(
                        "no win32 external-memory import on this device — demoting the decoder"
                    );
                    self.force_software.store(true, Ordering::Relaxed);
                }
                return Ok(false);
            }
            // Native Vulkan Video: decoded on the presenter's own device —
            // present is views + CSC, no import step. Same failure-streak demotion.
            // A drained/demoted frame drops through the arm below — its guard
            // still returns the decoder's slot.
            DecodedImage::NativeVk(v) if !self.health.demoted(Lane::Vulkan) => {
                self.hdr = v.color.is_pq();
                self.hdr_untonemapped = false;
                // The native lane first: a copy of the picture as the window's buffer.
                let res = match presenter.present_native_vk(v, pts_ns, decoded_ns) {
                    crate::vk::NativeVkOutcome::Shown => Ok(Presented::Shown),
                    crate::vk::NativeVkOutcome::Dropped => Ok(Presented::Stale),
                    crate::vk::NativeVkOutcome::Declined(v) => {
                        presenter.present(window, FrameInput::NativeVk(v), overlay)
                    }
                };
                (Rung::Hardware(Lane::Vulkan), res.map(Shown::of))
            }
            DecodedImage::NativeVk(_) => return Ok(false), // demoted — drain until rebuild
        };
        match res {
            Ok(Shown::Busy(image, on)) => {
                self.hold_busy(on, image, pts_ns, decoded_ns, due_ns);
                Ok(false)
            }
            Ok(shown) => {
                let shown = matches!(shown, Shown::Yes);
                self.health.presented(rung, shown);
                Ok(shown)
            }
            // Import/CSC failure is survivable — a hardware streak means this box
            // cannot do the hw path. A lost device is not survivable and must not demote.
            Err(e) if device_lost(&e) => Err(e).context("GPU device lost"),
            Err(e) => {
                if self.health.failed(rung, &e) {
                    tracing::warn!("demoting the decoder");
                    self.force_software.store(true, Ordering::Relaxed);
                }
                Ok(false)
            }
        }
    }

    /// No glass stamps: the submit instant stands in for the display time — for the
    /// audio plane's e2e and the HUD. The latch clock keeps its seed: a submit
    /// instant is not a latch, and the drain runs free without stamps.
    fn note_submitted(&mut self, pts_ns: u64, decoded_ns: u64) {
        let displayed_ns = session::now_ns();
        // Same hand-off as the glass-stamped branch. Anchored on submit, so it
        // understates the video leg by up to a refresh: inside the audio deadband.
        self.publish_e2e(self.clock_offset_ns(), displayed_ns, pts_ns);
        if let Some(c) = &self.connector {
            c.hud().note_displayed(pts_ns, decoded_ns, 0, displayed_ns);
        }
    }

    /// Host↔client clock offset, `0` until Connected. Loaded per use so a mid-stream
    /// re-sync keeps e2e honest after an NTP step.
    fn clock_offset_ns(&self) -> i64 {
        self.clock_offset
            .as_ref()
            .map_or(0, |o| o.load(Ordering::Relaxed))
    }

    /// Hand the audio plane the video leg's e2e for one displayed frame.
    fn publish_e2e(&self, clock_offset_ns: i64, displayed_ns: u64, pts_ns: u64) {
        if let (Some(e2e), Some(c)) = (
            e2e_ns(clock_offset_ns, displayed_ns, pts_ns),
            &self.video_e2e,
        ) {
            c.store(e2e, Ordering::Relaxed);
        }
    }
}

/// One second of presents, closed into the HUD and the presenter line once a second.
pub(super) struct PresentWindow {
    /// Per present: D3D11 import lookup (0 off that lane) and `vkQueueSubmit` wall time,
    /// for the presenter window line.
    import_us: Vec<u32>,
    submit_us: Vec<u32>,
    /// Per present: the in-flight fence wait, `vkAcquireNextImageKHR`, `vkQueuePresentKHR`.
    fence_us: Vec<u32>,
    acquire_us: Vec<u32>,
    present_us: Vec<u32>,
    /// The overlay window (`NativeClient::hud`) closes here once a second.
    start: Instant,
    /// This window's latch misses (glass later than one panel period past submit plus
    /// the applied lead). Adaptive margin's error signal.
    misses: u32,
    /// This window's frames shown before the latch they were aimed at.
    early: u32,
    out_max: usize,
    /// Consecutive on-glass spacings this window, in whole panel periods: `[0, 1, 2, 3, 4, 5+]`.
    /// The mode is the expected step; everything else is judder.
    steps: [u32; 6],
    /// Non-blocking presents that came back busy this window: [fence, acquire].
    busy: [u32; 2],
    /// Host repeats taken in: shown, and kept out of the cadence clock.
    repeats: u32,
    /// Run-loop passes.
    ticks: u32,
    /// Per shown frame: |on-glass spacing − source spacing| to the frame before it, µs.
    cadence_err_us: Vec<u32>,
    /// Samples whose stamp the engine took itself.
    exact: u32,
    /// Per shown frame: how far its on-glass spacing sat from a whole number of
    /// refreshes, µs.
    grid_err_us: Vec<u32>,
}

impl PresentWindow {
    pub(super) fn new() -> PresentWindow {
        PresentWindow {
            import_us: Vec::with_capacity(256),
            submit_us: Vec::with_capacity(256),
            fence_us: Vec::with_capacity(256),
            acquire_us: Vec::with_capacity(256),
            present_us: Vec::with_capacity(256),
            start: Instant::now(),
            misses: 0,
            early: 0,
            out_max: 0,
            steps: [0; 6],
            busy: [0; 2],
            repeats: 0,
            ticks: 0,
            cadence_err_us: Vec::with_capacity(256),
            exact: 0,
            grid_err_us: Vec::with_capacity(256),
        }
    }

    /// The last present's wall times, from the presenter.
    fn push(&mut self, presenter: &Presenter) {
        let (import_us, submit_us) = presenter.last_timings();
        self.import_us.push(import_us);
        self.submit_us.push(submit_us);
        let (fence_us, acquire_us, present_us) = presenter.last_waits();
        self.fence_us.push(fence_us);
        self.acquire_us.push(acquire_us);
        self.present_us.push(present_us);
    }

    /// `[import, submit, fence, acquire, present]` over the window, cleared for the next.
    fn take_timings(&mut self) -> [hud::Summary; 5] {
        [
            &mut self.import_us,
            &mut self.submit_us,
            &mut self.fence_us,
            &mut self.acquire_us,
            &mut self.present_us,
        ]
        .map(|v| {
            let s = hud::Summary::of(v);
            v.clear();
            s
        })
    }

    fn reset_counts(&mut self) {
        self.misses = 0;
        self.early = 0;
        self.out_max = 0;
        self.steps = [0; 6];
        self.busy = [0; 2];
        self.repeats = 0;
        self.ticks = 0;
        self.exact = 0;
    }
}

/// Display time against the frame's host capture stamp, in the host clock. `None` for a
/// non-positive or implausible (10 s or more) figure, which the audio plane must not chase.
fn e2e_ns(clock_offset_ns: i64, displayed_ns: u64, pts_ns: u64) -> Option<u64> {
    let e2e = (displayed_ns as i128 + clock_offset_ns as i128 - pts_ns as i128).max(0) as u64;
    (e2e > 0 && e2e < 10_000_000_000).then_some(e2e)
}

/// Which failure policy a lane's present runs under.
#[derive(Clone, Copy)]
enum Rung {
    /// A hardware lane. A failure streak demotes the decoder.
    Hardware(Lane),
    /// The last rungs: nothing left to demote to.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    PyroWave,
    Software,
}

/// Where a hardware lane's pictures come from. Each lane demotes the decoder once.
#[derive(Clone, Copy, PartialEq)]
enum Lane {
    /// Imported from the decoder's API: a VAAPI or V4L2 dma-buf, a D3D11 texture.
    Import,
    /// Decoded on the presenter's own device by Vulkan Video.
    Vulkan,
}

impl Lane {
    fn name(self) -> &'static str {
        match self {
            Lane::Import => "hardware",
            Lane::Vulkan => "native vulkan",
        }
    }
}

/// What a present did with its frame, whatever the lane.
enum Shown {
    Yes,
    /// Swapchain out of date; recreated, frame dropped.
    Stale,
    /// No swapchain image yet: the frame comes back for a retry.
    Busy(Option<DecodedImage>, crate::vk::BusyOn),
}

impl Shown {
    fn of(p: Presented<'_>) -> Shown {
        match p {
            Presented::Shown => Shown::Yes,
            Presented::Stale => Shown::Stale,
            Presented::Busy(input, on) => Shown::Busy(input.into_image(), on),
        }
    }
}

/// Present-failure streaks. A hardware streak of three demotes the decoder, once per lane
/// per session; the last rungs warn on the first failure of a streak and stay quiet until a
/// present succeeds.
#[derive(Default)]
pub(super) struct PresentHealth {
    hw_fails: u32,
    /// Lanes the decoder was told to leave (a failure streak, or no import support). Such a
    /// lane drains until the pump rebuilds; the lane it demotes onto still presents.
    demoted: Vec<Lane>,
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    pyro_warned: bool,
    cpu_warned: bool,
}

impl PresentHealth {
    /// A present that did not fail. `shown` false is a stale swapchain: the frame dropped.
    fn presented(&mut self, rung: Rung, shown: bool) {
        match rung {
            Rung::Hardware(_) if shown => self.hw_fails = 0,
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            Rung::PyroWave if shown => self.pyro_warned = false,
            Rung::Software if shown => self.cpu_warned = false,
            _ => {}
        }
    }

    /// A present failed short of device loss. `true` when the decoder demotes now. A
    /// window-system error, or one on a lane already demoted, says nothing about the
    /// decoder: it never counts toward the streak.
    fn failed(&mut self, rung: Rung, e: &anyhow::Error) -> bool {
        match rung {
            Rung::Hardware(lane) => {
                let counts = !crate::vk::PresentError::is_window(e) && !self.demoted(lane);
                if counts {
                    self.hw_fails += 1;
                }
                tracing::warn!(error = %format!("{e:#}"), fails = self.hw_fails,
                    "{} present failed", lane.name());
                counts && self.hw_fails >= 3 && self.demote(lane)
            }
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            Rung::PyroWave => {
                if !std::mem::replace(&mut self.pyro_warned, true) {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "pyrowave present failed — suppressing repeats until it recovers"
                    );
                }
                false
            }
            Rung::Software => {
                if !std::mem::replace(&mut self.cpu_warned, true) {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "software present failed — suppressing repeats until it recovers"
                    );
                }
                false
            }
        }
    }

    /// `lane`'s pictures drain until the pump rebuilds the decoder.
    fn demoted(&self, lane: Lane) -> bool {
        self.demoted.contains(&lane)
    }

    /// Demote `lane` once per session: `true` on the call that does. The next lane's
    /// streak starts over.
    fn demote(&mut self, lane: Lane) -> bool {
        if self.demoted(lane) {
            return false;
        }
        self.demoted.push(lane);
        self.hw_fails = 0;
        true
    }
}

impl StreamState {
    /// The presenter found `on` busy for `image`: it goes back for the next pass and the
    /// pace step's busy row holds the loop's wake. Newest-wins drops it if a fresher frame
    /// lands meanwhile.
    pub(super) fn hold_busy(
        &mut self,
        on: crate::vk::BusyOn,
        image: Option<DecodedImage>,
        pts_ns: u64,
        decoded_ns: u64,
        due_ns: i64,
    ) {
        let returned = image.is_some();
        if let Some(image) = image {
            self.store.put_back(Paced {
                frame: DecodedFrame {
                    pts_ns,
                    decoded_ns,
                    // Counted at intake; nothing reads the flag past it.
                    repeat: false,
                    image,
                },
                due_ns,
            });
        }
        self.pace.note_busy(&mut self.slots, on, returned);
        self.win.busy[on as usize] += 1;
    }

    /// The run loop's sleep: the pace step's wait over the store as it stands.
    pub(super) fn wake_timeout(&self) -> Duration {
        let paced = self.store.is_smoothing() || self.pacer.paces_latency();
        self.pace
            .wait(session::now_ns(), self.front_due_ns(), paced)
    }
}

impl StreamState {
    /// The engine's own refresh: its cycle is the grid presents quantize to, where the
    /// display mode's rate is a rounded claim, and an unbounded interval is variable
    /// refresh by the engine's word.
    pub(super) fn note_engine_refresh(&mut self, cycle_ns: u64, interval_ns: u64) {
        // 1 to 100 ms: 1000 Hz down to 10 Hz.
        if (1_000_000..=100_000_000).contains(&cycle_ns) {
            self.mode_period_ns = cycle_ns;
        }
        self.cadence.note_engine_variable(interval_ns == u64::MAX);
    }

    /// Re-seed the latch grid, VRR verdict and pacer anchor from the window's current
    /// display mode. Re-anchoring costs one frame; measured jitter survives, because
    /// that describes the link.
    pub(super) fn relearn_grid(&mut self, window: &sdl3::video::Window) {
        let hz = window
            .get_display()
            .and_then(|d| d.get_mode())
            .map(|m| m.refresh_rate.round().max(0.0) as u32)
            .unwrap_or(0);
        if hz > 0 {
            self.clock = LatchClock::new(hz);
            self.mode_period_ns = 1_000_000_000 / u64::from(hz);
        }
        self.cadence.reset();
        self.pacer.reset();
        self.slots.reset_margin();
        self.win.misses = 0;
        tracing::info!(
            refresh_hz = hz,
            "display changed — relearning the latch grid"
        );
    }

    /// Focus or fullscreen changed under the window: the compositor may have switched
    /// variable refresh on or off. The panel grid stands; the verdict is re-measured.
    pub(super) fn forget_refresh_verdict(&mut self) {
        self.cadence.reset();
    }
}

/// One frame at `refresh_hz`, in ns — the source's nominal interval, and the cadence
/// cushion's ceiling.
///
/// The negotiated stream mode's refresh is the only source-rate signal a client has.
/// Measured fps sags when the transport is struggling, which is when a ceiling
/// derived from it would license a bigger hold.
///
/// `0` = "native", which the host resolves to this client's reported display rate.
/// Neither known falls back to 60 Hz, the same last resort [`native_mode`]'s caller takes.
pub(super) fn frame_interval_ns(refresh_hz: u32, fallback_hz: u32) -> i64 {
    let hz = match (refresh_hz, fallback_hz) {
        (0, 0) => 60,
        (0, f) => f,
        (r, _) => r,
    };
    1_000_000_000 / i64::from(hz)
}

/// Whether a present error is `VK_ERROR_DEVICE_LOST` in its chain. A lost device is
/// unrecoverable by spec — every object on it is dead, and demote-to-software would
/// rebuild the decoder against that same dead device. Fail the session and let the
/// shell relaunch.
pub(super) fn device_lost(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<ash::vk::Result>() == Some(&ash::vk::Result::ERROR_DEVICE_LOST))
}

/// Overlay changes no present has carried to the glass yet. A still host desktop sends
/// no frames, so an opened ring or a new OSD line would stay invisible while the ring
/// holds the pad, and the menu would read as frozen.
#[derive(Default)]
pub(super) struct OverlayDamage {
    image: Option<ash::vk::Image>,
    dirty: bool,
    video_at: Option<Instant>,
}

impl OverlayDamage {
    /// Video quiet this long hands the overlay its own presents: longer than a live
    /// stream's frame gap, short enough that the ring opens without a visible lag.
    const VIDEO_QUIET: Duration = Duration::from_millis(100);

    /// Once per pass, after `Overlay::frame`. A re-render lands in the other ring slot.
    pub(super) fn rendered(&mut self, image: Option<ash::vk::Image>) {
        self.dirty |= image != self.image;
        self.image = image;
    }

    /// A video present composited the current overlay.
    pub(super) fn video_presented(&mut self, now: Instant) {
        self.dirty = false;
        self.video_at = Some(now);
    }

    /// Browsing: the overlay changed since the last present.
    pub(super) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// The overlay changed over a picture that has gone still: present it once.
    pub(super) fn take_due(&mut self, now: Instant) -> bool {
        let still = self
            .video_at
            .is_some_and(|t| now.duration_since(t) >= Self::VIDEO_QUIET);
        let due = self.dirty && still;
        self.dirty &= !due;
        due
    }
}

/// Advance the stats-overlay tier and re-render the OSD immediately from the last
/// window (waiting for the next Stats event would lag the trigger by up to 1 s).
pub(super) fn bump_stats_tier(verbosity: &mut StatsVerbosity, stream: &mut Option<StreamState>) {
    *verbosity = verbosity.next();
    if let Some(st) = stream {
        render_osd(st, *verbosity);
    }
}

/// The presenter's own counters for one window: what the `present:` line reports.
pub(super) struct PresentCounters {
    pub(super) mode: &'static str,
    pub(super) vrr: Cadence,
    pub(super) smoothing: bool,
    pub(super) q_drop: u32,
    pub(super) gated: u32,
    pub(super) forced: u32,
    /// Wake-forwarder displacements: the loop stalled two frame intervals.
    pub(super) forwarded: u32,
    /// (flipped without a copy, shown) this window, by the compositor's own word; `None`
    /// where it never says (KWin before 6.8, Windows, macOS).
    pub(super) scanout: Option<(u32, u32)>,
}

/// Close the overlay window: the connector's snapshot plus what only the presenter knows,
/// then the OSD and the stdout lines. Returns the display split's p50s (ms) for the log.
pub(super) fn close_window(
    st: &mut StreamState,
    presenter: &Presenter,
    present: &PresentCounters,
    replaced: u32,
    tier: StatsVerbosity,
) -> (f32, f32) {
    let Some(c) = st.connector.clone() else {
        return (0.0, 0.0);
    };
    // Replaced before display, dropped from a full smoothing queue, or displaced in the
    // wake forwarder: decoded, never shown.
    c.hud().note_skipped(
        replaced
            .saturating_add(present.q_drop)
            .saturating_add(present.forwarded),
        0,
    );
    let mut snap = c.hud_snapshot();
    snap.decoder = st.facts.decoder.to_string();
    snap.hdr = hdr_shown(st.hdr, presenter.hdr_active(), st.hdr_untonemapped);
    snap.preset = st.preset.clone();
    snap.on_glass = presenter.present_timing_active();
    let prev = std::mem::replace(&mut st.health_seen, st.facts.health);
    snap.extras = desktop_extras(present, st.facts.health, prev, session::codec_fallbacks());
    // The field bundle's per-second record, whatever the HUD tier: a report with no
    // stats line cannot say where its frames went.
    let text = hud::join(
        &hud_lines(&snap, StatsVerbosity::Detailed, true, st.last_video),
        " | ",
    );
    tracing::info!(target: "stats", "{text}");
    if tier != StatsVerbosity::Off {
        emit(SessionLine::Stats {
            text: &text,
            snap: &snap,
        });
    }
    let split = (
        snap.pace.p50_us as f32 / 1000.0,
        snap.latch.p50_us as f32 / 1000.0,
    );
    st.last_snap = Some(snap);
    render_osd(st, tier);
    split
}

/// Re-render the OSD from the last closed window at `tier`.
pub(super) fn render_osd(st: &mut StreamState, tier: StatsVerbosity) {
    st.osd = match &st.last_snap {
        Some(s) => hud_lines(s, tier, st.params.advanced_stats, st.last_video),
        None => Vec::new(),
    };
}

/// [`hud::format`], led by the size the picture decodes at when that is not the host's mode:
/// `1920×1080@60 · host renders 3840×2160`.
pub(super) fn hud_lines(
    s: &StatsSnapshot,
    tier: StatsVerbosity,
    advanced: bool,
    picture: Option<(u32, u32)>,
) -> Vec<HudLine> {
    let mut lines = hud::format(s, tier, advanced);
    let Some((w, h)) = picture.filter(|&p| p != (s.width, s.height)) else {
        return lines;
    };
    let mode = format!("{}×{}@{}", s.width, s.height, s.refresh_hz);
    if let Some(l) = lines.iter_mut().find(|l| l.fields.first() == Some(&mode)) {
        l.fields[0] = format!("{w}×{h}@{}", s.refresh_hz);
        let host = pf_l10n::client::overlay_host_renders(s.width, s.height);
        l.fields.insert(1, host);
        l.text = l.fields.join(" · ");
    }
    lines
}

/// How the stream reaches the screen. No present arm sets `untonemapped` today; the tag
/// stays so a lane that bypasses CSC can say so rather than claim a tone-map.
pub(super) fn hdr_shown(stream_hdr: bool, display_hdr: bool, untonemapped: bool) -> hud::Hdr {
    match (stream_hdr, display_hdr, untonemapped) {
        (false, ..) => hud::Hdr::Sdr,
        (true, true, _) => hud::Hdr::Hdr,
        (true, false, true) => hud::Hdr::Untonemapped,
        (true, false, false) => hud::Hdr::ToneMapped,
    }
}

/// Lines only the desktop measures: the live present path, decode integrity over this window
/// (`health` against `prev`), and this process's codec fallbacks.
pub(super) fn desktop_extras(
    p: &PresentCounters,
    health: Option<DecodeHealth>,
    prev: Option<DecodeHealth>,
    codec_fallbacks: u64,
) -> Vec<hud::Extra> {
    let mut out = Vec::new();
    if !p.mode.is_empty() {
        use pf_l10n::desktop as d;
        let mut t = d::desktop_overlay_present(p.mode);
        // Only once measured: an unproven "vrr no" would be a claim, not a reading.
        match p.vrr {
            Cadence::Unknown => {}
            Cadence::Fixed => t.push_str(&format!(" · {}", d::desktop_overlay_vrr_no())),
            Cadence::Variable => t.push_str(&format!(" · {}", d::desktop_overlay_vrr_yes())),
        }
        if p.smoothing {
            t.push_str(&format!(" · {}", d::desktop_overlay_smoothing()));
        }
        // The compositor's word, only where it gives one.
        let path: Option<String> = match p.scanout {
            Some((zc, shown)) if shown > 0 && zc == shown => {
                Some(d::desktop_overlay_scanout().into())
            }
            Some((0, shown)) if shown > 0 => Some(d::desktop_overlay_composited().into()),
            Some((zc, shown)) if shown > 0 => Some(d::desktop_overlay_scanout_share(format!(
                "{}%",
                zc * 100 / shown
            ))),
            _ => None,
        };
        if let Some(path) = path {
            t.push_str(&format!(" · {path}"));
        }
        for (name, n) in [
            ("qdrop", p.q_drop),
            ("fwd", p.forwarded),
            ("gated", p.gated),
            ("forced", p.forced),
        ] {
            if n > 0 {
                t.push_str(&format!(" · {name} {n}"));
            }
        }
        out.push(hud::Extra {
            text: t,
            tier: StatsVerbosity::Detailed,
            advanced_only: false,
            role: hud::Role::Muted,
        });
    }
    // A lane that cannot see damage says nothing; one that looked and saw none also says
    // nothing; one with half its detectors says so every window.
    if let Some(h) = health {
        let base = prev.unwrap_or_default();
        let damaged = h.damaged.saturating_sub(base.damaged);
        let refused = h.refused.saturating_sub(base.refused);
        let failed = h.failed.saturating_sub(base.failed);
        let mut parts = Vec::new();
        if damaged > 0 {
            parts.push(format!("damaged {damaged}"));
        }
        if refused > 0 {
            parts.push(format!("refused {refused}"));
        }
        if failed > 0 {
            parts.push(format!("driver-failed {failed}"));
        }
        if h.run > 0 {
            parts.push(format!("run {}", h.run));
        }
        // Session-cumulative: a 1 Hz sample of `run` misses the worst moment.
        if h.worst_run > h.run {
            parts.push(format!("worst run {}", h.worst_run));
        }
        if !h.status_queries {
            parts.push("no driver status".into());
        }
        if !parts.is_empty() {
            let hurt = damaged + refused + failed > 0 || h.run > 0;
            out.push(hud::Extra {
                text: format!("integrity: {}", parts.join(" · ")),
                tier: StatsVerbosity::Detailed,
                advanced_only: true,
                role: if hurt {
                    hud::Role::Warn
                } else {
                    hud::Role::Muted
                },
            });
        }
    }
    if codec_fallbacks > 0 {
        out.push(hud::Extra::detail(format!(
            "codec_fallbacks {codec_fallbacks}"
        )));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three hardware failures in a row demote the decoder, once per lane; a shown frame
    /// ends the streak. The lane it demotes onto still presents, on a streak of its own.
    #[test]
    fn a_hardware_failure_streak_demotes_each_lane_once() {
        let e = anyhow::anyhow!("import");
        let hw = Rung::Hardware(Lane::Import);
        let mut h = PresentHealth::default();
        assert!(!h.failed(hw, &e) && !h.failed(hw, &e));
        h.presented(hw, true);
        assert!(!h.failed(hw, &e) && !h.failed(hw, &e));
        assert!(h.failed(hw, &e), "the third in a row demotes");
        assert!(h.demoted(Lane::Import));
        assert!(!h.failed(hw, &e), "a demoted lane is told once");
        assert!(!h.demote(Lane::Import));

        let vk = Rung::Hardware(Lane::Vulkan);
        assert!(!h.demoted(Lane::Vulkan), "the lane below still presents");
        assert!(
            !h.failed(vk, &e) && !h.failed(vk, &e),
            "its streak starts over"
        );
        assert!(h.failed(vk, &e));
        assert!(!h.demote(Lane::Vulkan));
    }

    /// A stale swapchain drops the frame: it is not the recovery that re-arms a last
    /// rung's warning.
    #[test]
    fn a_stale_present_keeps_a_last_rung_quiet() {
        let e = anyhow::anyhow!("upload");
        let mut h = PresentHealth::default();
        assert!(!h.failed(Rung::Software, &e));
        h.presented(Rung::Software, false);
        assert!(h.cpu_warned, "stale is not a recovery");
        h.presented(Rung::Software, true);
        assert!(!h.cpu_warned);
    }

    /// The audio plane chases only a plausible video leg.
    #[test]
    fn e2e_is_published_only_inside_ten_seconds() {
        assert_eq!(e2e_ns(0, 150, 100), Some(50));
        // The host clock runs ahead: the offset brings display time into it.
        assert_eq!(e2e_ns(1_000, 150, 1_100), Some(50));
        assert_eq!(e2e_ns(0, 100, 150), None, "negative clamps to nothing");
        assert_eq!(e2e_ns(0, 100, 100), None);
        assert_eq!(e2e_ns(0, 10_000_000_100, 100), None);
    }

    #[test]
    fn overlay_damage_presents_a_changed_overlay_only_over_a_still_picture() {
        use ash::vk::Handle as _;
        let (a, b) = (ash::vk::Image::from_raw(1), ash::vk::Image::from_raw(2));
        let quiet = OverlayDamage::VIDEO_QUIET;
        let t0 = Instant::now();
        let mut d = OverlayDamage::default();
        // No video yet: nothing to present over.
        d.rendered(Some(a));
        assert!(!d.take_due(t0 + quiet));
        // Live video carries the change.
        d.video_presented(t0);
        d.rendered(Some(b));
        assert!(!d.take_due(t0 + quiet / 2));
        // The picture went still: the change presents once.
        assert!(d.take_due(t0 + quiet));
        assert!(!d.take_due(t0 + quiet * 2));
        // An unchanged overlay does not; closing it does.
        d.rendered(Some(b));
        assert!(!d.take_due(t0 + quiet * 2));
        d.rendered(None);
        assert!(d.take_due(t0 + quiet * 2));
    }

    /// Browsing presents each overlay change once; an idle console hands back the same image.
    #[test]
    fn overlay_damage_browse_presents_only_a_changed_overlay() {
        use ash::vk::Handle as _;
        let (a, b) = (ash::vk::Image::from_raw(1), ash::vk::Image::from_raw(2));
        let mut d = OverlayDamage::default();
        d.rendered(Some(a));
        assert!(d.take_dirty());
        d.rendered(Some(a));
        assert!(!d.take_dirty());
        d.rendered(Some(b));
        assert!(d.take_dirty());
    }

    /// Cadence cushion is bounded by the source's frame interval, not the panel's.
    #[test]
    fn the_cadence_interval_comes_from_the_stream_mode_not_the_panel() {
        assert_eq!(frame_interval_ns(120, 60), 8_333_333);
        assert_eq!(frame_interval_ns(60, 165), 16_666_666);
        // A `0 = native` request is resolved by the host to the display this client
        // reported, so that display's rate is what it will produce.
        assert_eq!(frame_interval_ns(0, 165), 6_060_606);
        // Neither known: 60 Hz, never an unbounded ceiling.
        assert_eq!(frame_interval_ns(0, 0), 16_666_666);
    }

    fn counters() -> PresentCounters {
        PresentCounters {
            mode: "fifo",
            vrr: Cadence::Unknown,
            smoothing: false,
            q_drop: 0,
            gated: 0,
            forced: 0,
            forwarded: 0,
            scanout: None,
        }
    }

    /// The present line names the live mode; counters show only when non-zero, and VRR only
    /// once measured.
    #[test]
    fn the_present_line_says_only_what_moved() {
        let quiet = desktop_extras(&counters(), None, None, 0);
        assert_eq!(quiet.len(), 1);
        assert_eq!(quiet[0].text, "present: fifo");
        // The compositor's word on scanout: whole, none, or a share; silence where it
        // never speaks or showed nothing.
        for (scanout, want) in [
            (Some((60, 60)), "present: fifo · scanout"),
            (Some((0, 60)), "present: fifo · composited"),
            (Some((30, 60)), "present: fifo · scanout 50%"),
            (Some((0, 0)), "present: fifo"),
        ] {
            let p = PresentCounters {
                scanout,
                ..counters()
            };
            assert_eq!(desktop_extras(&p, None, None, 0)[0].text, want);
        }
        assert!(
            !quiet[0].advanced_only,
            "Standard Detailed shows the present path too"
        );
        let busy = PresentCounters {
            vrr: Cadence::Variable,
            smoothing: true,
            q_drop: 2,
            gated: 7,
            forced: 1,
            ..counters()
        };
        assert_eq!(
            desktop_extras(&busy, None, None, 0)[0].text,
            "present: fifo · vrr yes · smoothing · qdrop 2 · gated 7 · forced 1"
        );
        let no_mode = PresentCounters {
            mode: "",
            ..counters()
        };
        assert!(desktop_extras(&no_mode, None, None, 0).is_empty());
        assert_eq!(
            desktop_extras(&no_mode, None, None, 2)[0].text,
            "codec_fallbacks 2"
        );
    }

    /// Integrity tells three quiet states apart: a lane that cannot see damage, one that
    /// looked and saw none, and one with only half its detectors.
    #[test]
    fn the_integrity_line_distinguishes_clean_from_unmeasurable() {
        let no_mode = PresentCounters {
            mode: "",
            ..counters()
        };
        let line = |now: DecodeHealth, prev: Option<DecodeHealth>| {
            desktop_extras(&no_mode, Some(now), prev, 0)
                .into_iter()
                .map(|e| e.text)
                .next()
        };
        assert!(desktop_extras(&no_mode, None, None, 0).is_empty());
        let clean = DecodeHealth {
            status_queries: true,
            ..DecodeHealth::default()
        };
        assert_eq!(line(clean, None), None);
        let radv = DecodeHealth {
            status_queries: false,
            ..clean
        };
        assert_eq!(
            line(radv, None).as_deref(),
            Some("integrity: no driver status")
        );
        let damaged = DecodeHealth {
            damaged: 4,
            failed: 2,
            run: 3,
            worst_run: 3,
            ..clean
        };
        assert_eq!(
            line(damaged, None).as_deref(),
            Some("integrity: damaged 4 · driver-failed 2 · run 3")
        );
        let recovered_hard = DecodeHealth {
            damaged: 4,
            worst_run: 40,
            ..clean
        };
        assert_eq!(
            line(recovered_hard, None).as_deref(),
            Some("integrity: damaged 4 · worst run 40")
        );
        let refusing = DecodeHealth {
            refused: 60,
            run: 60,
            worst_run: 60,
            ..clean
        };
        assert_eq!(
            line(refusing, None).as_deref(),
            Some("integrity: refused 60 · run 60")
        );
        // Windowed against the last window's cumulative counters.
        let later = DecodeHealth {
            damaged: 6,
            ..clean
        };
        let before = DecodeHealth {
            damaged: 4,
            ..clean
        };
        assert_eq!(
            line(later, Some(before)).as_deref(),
            Some("integrity: damaged 2")
        );
        let e = desktop_extras(&no_mode, Some(damaged), None, 0);
        assert!(e[0].advanced_only && e[0].role == hud::Role::Warn);
    }

    #[test]
    fn hdr_tag_follows_the_swapchain() {
        assert_eq!(hdr_shown(false, true, false), hud::Hdr::Sdr);
        assert_eq!(hdr_shown(true, true, false), hud::Hdr::Hdr);
        assert_eq!(hdr_shown(true, false, false), hud::Hdr::ToneMapped);
        assert_eq!(hdr_shown(true, false, true), hud::Hdr::Untonemapped);
    }

    /// A picture at the host's mode reads as the mode alone; one the host scaled down leads
    /// with its own size and names the mode after it.
    #[test]
    fn the_mode_line_names_the_host_size_beside_a_smaller_picture() {
        let s = StatsSnapshot {
            width: 3840,
            height: 2160,
            refresh_hz: 60,
            ..Default::default()
        };
        let first = |picture, tier| hud_lines(&s, tier, false, picture)[0].text.clone();
        let normal = StatsVerbosity::Normal;
        assert!(first(None, normal).starts_with("3840×2160@60"));
        assert_eq!(first(Some((3840, 2160)), normal), first(None, normal));
        let scaled = first(Some((1920, 1080)), normal);
        assert!(
            scaled.starts_with("1920×1080@60 · host renders 3840×2160"),
            "{scaled}"
        );
        let advanced = hud_lines(&s, StatsVerbosity::Detailed, true, Some((1920, 1080)));
        assert_eq!(
            advanced[0].fields[..2],
            ["1920×1080@60", "host renders 3840×2160"]
        );
        // Compact names no size at all.
        let compact = StatsVerbosity::Compact;
        assert_eq!(first(Some((1920, 1080)), compact), first(None, compact));
    }
}
