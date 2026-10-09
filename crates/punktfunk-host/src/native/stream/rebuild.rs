//! Everything that swaps the pipeline under a running stream: a Gaming↔Desktop session switch,
//! a client mode switch, the Windows topology re-assert, capture loss, and a source that changed
//! size with no client Reconfigure. Each path ends in [`StreamState::adopt_pipeline`].

#[cfg(target_os = "linux")]
use super::cursor::settle_portal_cursor;
#[cfg(target_os = "linux")]
use super::pipeline::reattach_pipeline;
use super::pipeline::{
    build_pipeline, build_pipeline_with_retry, is_permanent_build_error, open_session_encoder,
    Pipeline,
};
use super::state::{announce_pipeline_gap, StreamState};
use super::*;
use crate::encode_recovery::{RebuildBudget, MAX_CAPTURE_REBUILDS, MAX_ENCODER_RESETS};

/// Isolated gamescope keeps its pinned injector and must not steal the shared backend
/// (last-write-wins). Everyone else gets the shared sender plus `set_backend_id`.
#[cfg(target_os = "linux")]
fn repoint_session_input(
    input_route: &super::input::InputRoute,
    shared: &std::sync::mpsc::Sender<punktfunk_core::input::InputEvent>,
    session: Option<&std::sync::mpsc::Sender<punktfunk_core::input::InputEvent>>,
    compositor: crate::vdisplay::Compositor,
    route: Option<&crate::vdisplay::GamescopeRoute>,
) {
    match session.filter(|_| crate::compositor_route::session_is_isolated(compositor, route)) {
        Some(tx) => input_route.set(tx.clone()),
        None => {
            input_route.set(shared.clone());
            crate::inject::set_backend_id(crate::vdisplay::input_backend_id(compositor));
        }
    }
}

impl StreamState {
    /// Follow the watcher's latest session switch: rebuild the backend in place, with the cursor
    /// plan of the compositor it switches to, and keep streaming.
    pub(super) fn on_session_switch(&mut self) {
        let mut switch = None;
        while let Ok(s) = self.session_rx.try_recv() {
            switch = Some(s);
        }
        let Some(sw) = switch else {
            return;
        };
        if sw.compositor == self.compositor {
            return;
        }
        tracing::info!(from = self.compositor.id(), to = sw.compositor.id(), kind = ?sw.kind,
            "session switch — rebuilding backend in place");
        // Only writer is not safety: `setenv` races every concurrent `getenv` in the process.
        crate::vdisplay::apply_session_env(&crate::vdisplay::ActiveSession {
            kind: sw.kind,
            env: sw.env,
            compositor_pid: None,
        });
        let switched_route = crate::vdisplay::resolve_gamescope_route(sw.compositor, false);
        #[cfg(target_os = "linux")]
        repoint_session_input(
            &self.input_route,
            &self.inj_shared_tx,
            self.inj_session_tx.as_ref(),
            sw.compositor,
            switched_route.as_ref(),
        );
        #[cfg(not(target_os = "linux"))]
        crate::inject::set_backend_id(crate::vdisplay::input_backend_id(sw.compositor));
        if matches!(
            sw.compositor,
            crate::vdisplay::Compositor::Kwin | crate::vdisplay::Compositor::Mutter
        ) {
            crate::vdisplay::settle_desktop_portal(sw.compositor);
        }
        // The pipeline below is built from this plan; a failed switch restores the old one.
        let (old_plan, old_composite) = (
            self.plan,
            (self.gamescope_composite, self.metadata_composite),
        );
        let hw_cursor = self.retarget_cursor_plan(sw.compositor, switched_route.as_ref());
        self.close_open_frame();
        let rebuilt = (|| -> Result<(Box<dyn crate::vdisplay::VirtualDisplay>, Pipeline)> {
            let mut new_vd = crate::vdisplay::open(sw.compositor)?;
            new_vd.set_hw_cursor(hw_cursor);
            new_vd.set_gamescope_route(switched_route.clone());
            new_vd.set_join_live(self.join_live);
            // The HDR verdict, as at session start: a switched-to gamescope launches in it.
            new_vd.set_hdr(self.plan.hdr);
            #[cfg(target_os = "linux")]
            new_vd.set_session_isolation(self.isolation.clone());
            let pipe = build_pipeline_with_retry(
                &mut new_vd,
                self.cur_mode,
                self.bitrate_kbps,
                self.bitrate_auto,
                self.bit_depth,
                self.enc_now(),
                self.plan,
                &self.quit,
                &self.stop,
                None,
                8,
                None,
                self.client_hdr,
                self.au_seq,
            )?;
            Ok((new_vd, pipe))
        })();
        match rebuilt {
            Ok((new_vd, pipe)) => {
                let built = pipe.bitrate_kbps;
                self.adopt_pipeline(pipe);
                self.adopt_built_bitrate(built);
                self.vd = new_vd;
                self.compositor = sw.compositor;
                #[cfg(target_os = "linux")]
                {
                    self.no_overlay_means_off_output =
                        settle_portal_cursor(&*self.vd, &mut self.metadata_composite);
                }
                self.next = std::time::Instant::now();
                tracing::info!(
                    compositor = self.compositor.id(),
                    "session switch — backend rebuilt, stream continues"
                );
            }
            Err(e) => {
                let chain = format!("{e:#}");
                let kind = if is_permanent_build_error(&chain) {
                    "permanent"
                } else {
                    "transient"
                };
                self.plan = old_plan;
                (self.gamescope_composite, self.metadata_composite) = old_composite;
                tracing::warn!(error = %chain, kind,
                    "session-switch rebuild failed — staying on the current backend");
            }
        }
    }

    /// The client's latest accepted mode: in place on Windows IDD-push, else a full rebuild. A
    /// failed rebuild stays on the current mode and tells the client so. PyroWave re-resolves
    /// its pin for the new mode at the session's bits per pixel, and an Automatic session keeps
    /// its rate inside the new bounds.
    pub(super) fn on_mode_switch(&mut self) {
        let mut want = None;
        while let Ok(m) = self.reconfig.try_recv() {
            want = Some(m);
        }
        let Some(new_mode) = want else {
            return;
        };
        tracing::info!(?new_mode, "rebuilding pipeline for mode switch");
        let resize_trace = crate::bringup::Trace::start("resize", self.resize_ms.clone());
        let mode_bitrate = if self.plan.codec == crate::encode::Codec::PyroWave {
            let running = self.bitrate_auto.then_some(self.bitrate_kbps);
            pyrowave_mode_kbps(running, &new_mode, &self.plan)
        } else {
            self.bitrate_kbps
        };
        self.close_open_frame();
        #[cfg(target_os = "windows")]
        let fast_done = self.plan.capture == crate::session_plan::CaptureBackend::IddPush
            && self.resize(new_mode, mode_bitrate, resize_trace.as_ref(), false);
        #[cfg(not(target_os = "windows"))]
        let fast_done = false;
        let mut built_bitrate = mode_bitrate;
        let enc_of = self.enc_now();
        let rebuilt = fast_done
            || match build_pipeline(
                &mut self.vd,
                new_mode,
                mode_bitrate,
                self.bitrate_auto,
                self.bit_depth,
                enc_of,
                self.plan,
                &self.quit,
                self.cur_display_gen,
                None,
                Some(resize_trace.as_ref()),
                self.client_hdr,
                self.au_seq,
            ) {
                Ok(next_pipe) => {
                    let old_display_gen = self.cur_display_gen;
                    built_bitrate = next_pipe.bitrate_kbps;
                    self.adopt_pipeline(next_pipe);
                    self.retire_replaced_gen(old_display_gen);
                    true
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), ?new_mode,
                        "mode-switch rebuild failed — staying on the current mode");
                    self.tell_delivered(self.delivered_mode(), true);
                    false
                }
            };
        if !rebuilt {
            return;
        }
        // The pixel rate changed, so what the encoder can hold changed with it.
        // The in-place Windows resize swaps the encoder without `adopt_pipeline`,
        // so this covers both halves.
        self.encoder_ceiling
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        // Covers the in-place Windows resize, which swaps the encoder without
        // `adopt_pipeline`: the new one has been handed nothing yet.
        self.retargeted = false;
        self.fec_pending = None;
        self.adopt_built_bitrate(built_bitrate);
        self.cur_mode = new_mode;
        self.next = std::time::Instant::now();
        self.enc_src = (self.frame.format, self.frame.width, self.frame.height);
        self.publish_delivered_mode(new_mode);
        self.inflight.clear();
        self.watchdog.on_au();
        self.last_forced_idr = Some(std::time::Instant::now());
        resize_trace.finish("pipeline_rebuilt");
        // Reconfigured clears baselines, not the straddling window or slow start.
        announce_pipeline_gap(
            &self.gap_tx,
            resize_trace.total_slot().load(Ordering::Relaxed),
        );
        // This resize moved the topology itself, so the watchdog's re-assert lands as a
        // bump the eviction check would read as somebody else's. Capture just rebuilt at
        // the new mode, which is what that check would do anyway.
        self.seen_reassert_gen = crate::windows::idd::topology_reassert_gen();
    }

    /// Only Windows IDD-push has a topology watchdog to follow.
    #[cfg(not(target_os = "windows"))]
    pub(super) fn on_topology_reassert(&mut self) -> Result<()> {
        Ok(())
    }

    /// An exclusive-topology eviction bounced the virtual display's modes: re-attach capture in
    /// place at the current mode, or rebuild the pipeline outright. Ending the session on an
    /// idle desktop would just loop the client.
    #[cfg(target_os = "windows")]
    pub(super) fn on_topology_reassert(&mut self) -> Result<()> {
        if self.plan.capture != crate::session_plan::CaptureBackend::IddPush {
            return Ok(());
        }
        let reassert_gen = crate::windows::idd::topology_reassert_gen();
        if reassert_gen == self.seen_reassert_gen {
            return Ok(());
        }
        self.seen_reassert_gen = reassert_gen;
        tracing::info!(
            "exclusive-topology eviction bounced the virtual display's modes — rebuilding \
             the capture attachment in place at the current mode"
        );
        let trace = crate::bringup::Trace::start("reassert-recover", self.resize_ms.clone());
        self.close_open_frame();
        if !self.resize(self.cur_mode, self.bitrate_kbps, trace.as_ref(), true) {
            // The in-place recovery proves the OS resumed presenting by waiting for a NEWER
            // frame, which an idle desktop never produces — so its failure is not evidence
            // the display is gone.
            let enc_of = self.enc_now();
            match build_pipeline(
                &mut self.vd,
                self.cur_mode,
                self.bitrate_kbps,
                self.bitrate_auto,
                self.bit_depth,
                enc_of,
                self.plan,
                &self.quit,
                self.cur_display_gen,
                None,
                Some(trace.as_ref()),
                self.client_hdr,
                self.au_seq,
            ) {
                Ok(next_pipe) => {
                    let old_display_gen = self.cur_display_gen;
                    // Same mode as before the bounce, so the built bitrate is the one
                    // already in force: nothing to adopt.
                    self.adopt_pipeline(next_pipe);
                    self.retire_replaced_gen(old_display_gen);
                }
                Err(e) => {
                    return Err(e).context(
                        "exclusive-topology eviction recovery failed, and the full \
                         pipeline rebuild after it failed too",
                    );
                }
            }
        }
        self.enc_src = (self.frame.format, self.frame.width, self.frame.height);
        self.inflight.clear();
        self.watchdog.on_au();
        self.last_forced_idr = Some(std::time::Instant::now());
        trace.finish("pipeline_rebuilt");
        announce_pipeline_gap(&self.gap_tx, trace.total_slot().load(Ordering::Relaxed));
        Ok(())
    }

    /// Capture failed. On Linux a dedicated game session whose game exited emits `game.exited`
    /// and ends cleanly (`Ok(false)`); otherwise rebuild within a budget, re-detecting the live
    /// compositor each attempt. `Err` = the rebuild budget or the rebuild count is exhausted.
    pub(super) fn on_capture_lost(&mut self, e: anyhow::Error) -> Result<bool> {
        #[cfg(not(target_os = "linux"))]
        let _ = (&self.cur_node_id, &self.game_life);
        #[cfg(target_os = "linux")]
        if self.launch.is_some()
            && crate::session_settings::get().session_on_game_exit
            && crate::vdisplay::launch_is_nested(self.compositor, self.gamescope_route.as_ref())
            && crate::vdisplay::dedicated_game_exited(self.cur_node_id)
        {
            tracing::info!("dedicated game session: the game exited — ending the session cleanly");
            if let Some(g) = self.game_life.as_ref() {
                crate::gamelease::report_exit(&g.shared());
            }
            crate::events::SessionEndReason::GameExited.latch(&self.end_reason);
            self.quit.store(true, Ordering::SeqCst);
            self.conn
                .close(punktfunk_core::quic::APP_EXITED_CLOSE_CODE, b"game exited");
            return Ok(false);
        }
        self.capture_rebuilds += 1;
        if self.capture_rebuilds > MAX_CAPTURE_REBUILDS {
            return Err(e).context("capture lost — rebuild attempts exhausted");
        }
        tracing::warn!(error = %format!("{e:#}"), rebuild = self.capture_rebuilds,
            "capture lost — rebuilding pipeline in place");
        // Retry within the budget while the QUIC keepalive holds the connection, re-detecting
        // the live compositor each attempt. The client stays connected, frozen on the last
        // frame, and resumes — no reconnect.
        let budget = RebuildBudget::start();
        if pf_host_config::config().compositor.is_some() {
            let active = crate::vdisplay::detect_active_session();
            if crate::vdisplay::compositor_for_kind(active.kind) != Some(self.compositor) {
                tracing::warn!(
                    pinned = self.compositor.id(),
                    live = ?active.kind,
                    "capture lost while PUNKTFUNK_COMPOSITOR pins the backend and the \
                     live session no longer matches it — the pin disables \
                     session-following, so this rebuild can only retry the pinned \
                     backend; remove the pin to let the stream follow session switches"
                );
            }
        }
        self.close_open_frame();
        let pipe = 'built: {
            // The import side broke under a display that is still up: re-attach to it
            // before creating another. On KWin a create is a new virtual output, and #1443
            // shows a burst of them wedging the compositor into placeholder screens.
            #[cfg(target_os = "linux")]
            if e.downcast_ref::<pf_capture::DisplayStillAlive>().is_some() {
                if let (Some(lease), Some(keepalive)) =
                    (self.lease.clone(), self.capturer.take_keepalive())
                {
                    let enc_of = self.enc_now();
                    match reattach_pipeline(
                        &mut self.vd,
                        lease,
                        keepalive,
                        self.cur_mode,
                        self.bitrate_kbps,
                        self.bitrate_auto,
                        self.bit_depth,
                        enc_of,
                        self.plan,
                        self.client_hdr,
                        self.au_seq,
                    ) {
                        Ok(p) => {
                            tracing::info!(
                                node_id = p.node_id,
                                "capture loss: re-attached to the live output — no new display"
                            );
                            break 'built p;
                        }
                        Err(e2) => tracing::warn!(error = %format!("{e2:#}"),
                            "capture loss: re-attach to the live output failed — creating another"),
                    }
                }
            }
            loop {
                if pf_host_config::config().compositor.is_none() {
                    self.retarget_to_live_session();
                }
                let _probe = budget.probe_scope();
                let enc_of = self.enc_now();
                match build_pipeline_with_retry(
                    &mut self.vd,
                    self.cur_mode,
                    self.bitrate_kbps,
                    self.bitrate_auto,
                    self.bit_depth,
                    enc_of,
                    self.plan,
                    &self.quit,
                    &self.stop,
                    self.cur_display_gen,
                    1,
                    None,
                    self.client_hdr,
                    self.au_seq,
                ) {
                    Ok(p) => break 'built p,
                    Err(e2) => {
                        if self.stop.load(Ordering::SeqCst) || budget.expired(Some(self.compositor))
                        {
                            return Err(e2).context(
                                "capture lost — no compositor came up within the rebuild budget",
                            );
                        }
                        tracing::warn!(error = %format!("{e2:#}"),
                        "capture lost — new session not up yet, retrying");
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
            }
        };
        let old_display_gen = self.cur_display_gen;
        let built = pipe.bitrate_kbps;
        self.adopt_pipeline(pipe);
        self.retire_replaced_gen(old_display_gen);
        self.enc_src = (self.frame.format, self.frame.width, self.frame.height);
        #[cfg(target_os = "linux")]
        {
            self.no_overlay_means_off_output =
                settle_portal_cursor(&*self.vd, &mut self.metadata_composite);
        }
        self.adopt_built_bitrate(built);
        self.enc.request_keyframe();
        self.last_forced_idr = Some(std::time::Instant::now());
        self.next = std::time::Instant::now();
        tracing::info!(
            compositor = self.compositor.id(),
            "capture loss: pipeline rebuilt — stream resumes"
        );
        Ok(true)
    }

    /// One capture-loss attempt's re-detection: follow the live session's compositor, opening a
    /// new backend when it changed, re-point input at it and re-derive the cursor plan.
    fn retarget_to_live_session(&mut self) {
        let active = crate::vdisplay::detect_active_session();
        crate::vdisplay::observe_session_instance(&active);
        let Some(c) = crate::vdisplay::compositor_for_kind(active.kind) else {
            return;
        };
        crate::vdisplay::apply_session_env(&active);
        let rebuilt_route = crate::vdisplay::resolve_gamescope_route(c, false);
        #[cfg(target_os = "linux")]
        repoint_session_input(
            &self.input_route,
            &self.inj_shared_tx,
            self.inj_session_tx.as_ref(),
            c,
            rebuilt_route.as_ref(),
        );
        #[cfg(not(target_os = "linux"))]
        crate::inject::set_backend_id(crate::vdisplay::input_backend_id(c));
        if c != self.compositor {
            if matches!(
                c,
                crate::vdisplay::Compositor::Kwin | crate::vdisplay::Compositor::Mutter
            ) {
                crate::vdisplay::settle_desktop_portal(c);
            }
            match crate::vdisplay::open(c) {
                Ok(v) => {
                    tracing::info!(
                        from = self.compositor.id(),
                        to = c.id(),
                        "capture loss: active session switched compositor — retargeting"
                    );
                    self.vd = v;
                    self.compositor = c;
                    self.vd.set_hdr(self.plan.hdr);
                }
                Err(e2) => tracing::warn!(error = %format!("{e2:#}"),
                    "capture loss: opening the newly-detected compositor failed — retrying"),
            }
        }
        // Also when only the gamescope route changed: Attach and Spawn differ in who draws.
        let hw_cursor = self.retarget_cursor_plan(self.compositor, rebuilt_route.as_ref());
        self.vd.set_hw_cursor(hw_cursor);
        self.vd.set_gamescope_route(rebuilt_route.clone());
        self.vd.set_join_live(self.join_live);
        #[cfg(target_os = "linux")]
        self.vd.set_session_isolation(self.isolation.clone());
    }

    /// The source changed format or size with no client Reconfigure: reopen the encoder at the
    /// delivered size and make it the session's mode, PyroWave's rate re-resolved for it as on
    /// a mode switch. `Ok(false)` = the reopen failed and the tick is spent on the backoff.
    pub(super) fn follow_source_mode(&mut self) -> Result<bool> {
        if self.enc_src == (self.frame.format, self.frame.width, self.frame.height) {
            return Ok(true);
        }
        let actual = self.delivered_mode();
        let src_kbps = if self.plan.codec == crate::encode::Codec::PyroWave {
            let running = self.bitrate_auto.then_some(self.bitrate_kbps);
            pyrowave_mode_kbps(running, &actual, &self.plan)
        } else {
            self.bitrate_kbps
        };
        self.close_open_frame();
        // The encoder's rate, not the wire budget: FEC, framing and audio ride on top of it.
        let ed = self.enc_now();
        let opened = open_session_encoder(
            &self.plan,
            &*self.capturer,
            &self.frame,
            (self.negotiated.width, self.negotiated.height),
            actual.refresh_hz,
            |_, _| ed.enc_kbps(src_kbps) as u64 * 1000,
            self.bit_depth,
            self.client_hdr,
            self.au_seq,
        )
        .with_context(|| {
            format!(
                "the capture source changed to {}x{} {:?} mid-session and the encoder could not \
                 be reopened at it",
                self.frame.width, self.frame.height, self.frame.format
            )
        });
        let new_enc = match opened {
            Ok((e, reframe)) => {
                self.adopt_reframe(reframe);
                e
            }
            Err(e) => {
                let Some(backoff) = self.watchdog.spend(self.interval) else {
                    return Err(e).context("encoder reopen at the source's new mode");
                };
                tracing::warn!(error = %format!("{e:#}"), reset = self.watchdog.resets(),
                    max = MAX_ENCODER_RESETS,
                    "reopening the encoder at the source's new mode failed — retrying");
                self.next = std::time::Instant::now() + backoff;
                std::thread::sleep(backoff);
                return Ok(false);
            }
        };
        tracing::info!(
            from = %format!("{}x{} {:?}", self.enc_src.1, self.enc_src.2, self.enc_src.0),
            to = %format!("{}x{} {:?}", self.frame.width, self.frame.height, self.frame.format),
            "the capture source changed mode mid-session with no client reconfigure — reopened \
             the encoder at the delivered size"
        );
        self.enc = new_enc;
        self.carry_pipelining();
        self.enc_src = (self.frame.format, self.frame.width, self.frame.height);
        // A ceiling learned from the encoder this one replaces: another geometry or format.
        self.encoder_ceiling
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        // The delivered mode is the session's now: a rebuild or topology re-assert
        // reopens at it instead of forcing the display back to the client's ask.
        self.cur_mode = actual;
        self.epoch = self.epoch.wrapping_add(1);
        self.adopt_built_bitrate(src_kbps);
        self.inflight.clear();
        self.watchdog.on_au();
        self.last_forced_idr = Some(std::time::Instant::now());
        self.live_mode.store(
            pack_mode(actual.width, actual.height, actual.refresh_hz),
            Ordering::Relaxed,
        );
        self.tell_delivered(actual, true);
        Ok(true)
    }
}
