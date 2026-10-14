//! Everything that swaps the pipeline under a running stream: a Gaming↔Desktop session switch,
//! a client mode switch, the Windows topology re-assert, capture loss, and a source that changed
//! size with no client Reconfigure. Each path builds the replacement and hands it to
//! [`StreamState::swap_pipeline`].

#[cfg(target_os = "linux")]
use super::cursor::settle_portal_cursor;
#[cfg(target_os = "linux")]
use super::pipeline::reattach_pipeline;
use super::pipeline::{
    build_pipeline, build_pipeline_with_retry, is_permanent_build_error, open_session_encoder,
    Pipeline,
};
use super::state::{announce_pipeline_gap, StreamState};
use super::swap::{Next, SwapReason};
use super::*;
use crate::encode_recovery::{RebuildBudget, MAX_CAPTURE_REBUILDS, MAX_ENCODER_RESETS};
use crate::native::bitrate::pyrowave_mode_kbps;
use crate::session_status::pack_mode;

/// How long a capture-loss rebuild waits after a failed attempt.
const LOSS_RETRY: std::time::Duration = std::time::Duration::from_millis(500);

/// Count a capture loss against [`MAX_CAPTURE_REBUILDS`]: `Err` once they are spent. `Ok`
/// carries the lost capturer's keepalive when the import side broke under a display that is
/// still up, for [`rebuild_after_capture_loss`] to re-attach with.
pub(crate) fn count_capture_loss(
    plane: crate::events::Plane,
    rebuilds: &mut u32,
    e: anyhow::Error,
    capturer: &mut dyn crate::capture::Capturer,
) -> Result<Option<Box<dyn Send>>> {
    *rebuilds += 1;
    if *rebuilds > MAX_CAPTURE_REBUILDS {
        return Err(e).context("capture lost — rebuild attempts exhausted");
    }
    tracing::warn!(error = %format!("{e:#}"), rebuild = *rebuilds, plane = plane.as_str(),
        "capture lost — rebuilding in place");
    Ok(e.downcast_ref::<pf_capture::DisplayStillAlive>()
        .and_then(|_| capturer.take_keepalive()))
}

/// One stream loop's side of [`rebuild_after_capture_loss`].
pub(crate) trait CaptureLossRebuild {
    /// What a rebuild hands the loop.
    type Built;
    fn plane(&self) -> crate::events::Plane;
    /// Capture again on the output the loss left up. `None` = it could not; the loop logs why.
    fn reattach(&mut self, keepalive: Box<dyn Send>) -> Option<Self::Built>;
    /// One attempt at a new source.
    fn build(&mut self) -> Result<Self::Built>;
    /// The session is ending: stop retrying.
    fn ending(&self) -> bool;
    /// The compositor the budget is judged on ([`RebuildBudget::expired`]).
    fn budget_compositor(&self) -> Option<crate::vdisplay::Compositor>;
}

/// Rebuild after a counted capture loss: re-attach with `keepalive` first, then build every
/// [`LOSS_RETRY`] until a source comes up, the session ends or `budget` runs out.
pub(crate) fn rebuild_after_capture_loss<L: CaptureLossRebuild>(
    l: &mut L,
    keepalive: Option<Box<dyn Send>>,
    budget: &RebuildBudget,
) -> Result<L::Built> {
    if let Some(built) = keepalive.and_then(|k| l.reattach(k)) {
        return Ok(built);
    }
    loop {
        let _probe = budget.probe_scope();
        match l.build() {
            Ok(built) => return Ok(built),
            Err(e) => {
                if l.ending() || budget.expired(l.budget_compositor()) {
                    return Err(e)
                        .context("capture lost — no source came up within the rebuild budget");
                }
                tracing::warn!(error = %format!("{e:#}"), plane = l.plane().as_str(),
                    "capture lost — no source up yet, retrying");
                std::thread::sleep(LOSS_RETRY);
            }
        }
    }
}

impl CaptureLossRebuild for StreamState {
    type Built = Pipeline;

    fn plane(&self) -> crate::events::Plane {
        self.conn.plane()
    }

    /// Re-attach before creating another display: on KWin a create is a new virtual output, and
    /// a burst of them wedges the compositor into placeholder screens.
    #[cfg(target_os = "linux")]
    fn reattach(&mut self, keepalive: Box<dyn Send>) -> Option<Pipeline> {
        let lease = self.lease.clone()?;
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
            self.clock.au_seq,
        ) {
            Ok(p) => {
                tracing::info!(
                    node_id = p.node_id,
                    "capture loss: re-attached to the live output — no new display"
                );
                Some(p)
            }
            Err(e2) => {
                tracing::warn!(error = %format!("{e2:#}"),
                    "capture loss: re-attach to the live output failed — creating another");
                None
            }
        }
    }

    /// Only a Linux capturer hands back a keepalive.
    #[cfg(not(target_os = "linux"))]
    fn reattach(&mut self, _keepalive: Box<dyn Send>) -> Option<Pipeline> {
        None
    }

    /// Re-detect the live compositor unless `PUNKTFUNK_COMPOSITOR` pins it, then build once.
    fn build(&mut self) -> Result<Pipeline> {
        if pf_host_config::config().compositor.is_none() {
            self.retarget_to_live_session();
        }
        let enc_of = self.enc_now();
        build_pipeline_with_retry(
            &mut self.vd,
            self.cur_mode,
            self.bitrate_kbps,
            self.bitrate_auto,
            self.bit_depth,
            enc_of,
            self.plan,
            self.life.quit_flag(),
            self.life.stop_flag(),
            self.cur_display_gen,
            1,
            None,
            self.client_hdr,
            self.clock.au_seq,
        )
    }

    fn ending(&self) -> bool {
        self.life.stopping()
    }

    /// The compositor this session runs on now: each attempt's re-detection can move it.
    fn budget_compositor(&self) -> Option<crate::vdisplay::Compositor> {
        Some(self.compositor)
    }
}

/// Isolated gamescope keeps its pinned injector and must not steal the shared backend
/// (last-write-wins). Everyone else gets the shared sender plus `set_backend_id`; a seat's
/// desktop keeps the backend its runner picked.
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
            crate::inject::set_backend_id(&crate::seats::session_switch::input_backend_id(
                compositor,
            ));
        }
    }
}

impl StreamState {
    /// Follow the latest session switch, the watcher's or the seat's: rebuild the backend in
    /// place, with the session's display request, the switch's launch and the cursor plan of the
    /// compositor it switches to, and keep streaming.
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
        // Being the only writer proves nothing: `setenv` races every concurrent `getenv` in the process.
        if let Some(env) = sw.env {
            crate::vdisplay::apply_session_env(&crate::vdisplay::ActiveSession {
                kind: sw.kind,
                env,
                compositor_pid: None,
            });
        }
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
            let params = crate::vdisplay::SessionParams {
                launch: sw.launch.clone(),
                ..self.reopen_params(hw_cursor, switched_route)
            };
            params.apply(&mut *new_vd);
            let pipe = build_pipeline_with_retry(
                &mut new_vd,
                self.cur_mode,
                self.bitrate_kbps,
                self.bitrate_auto,
                self.bit_depth,
                self.enc_now(),
                self.plan,
                self.life.quit_flag(),
                self.life.stop_flag(),
                None,
                8,
                None,
                self.client_hdr,
                self.clock.au_seq,
            )?;
            Ok((new_vd, pipe))
        })();
        match rebuilt {
            Ok((new_vd, pipe)) => {
                let built = pipe.bitrate_kbps;
                self.swap_pipeline(Next::Pipeline(pipe), SwapReason::SessionSwitch);
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
                self.life.quit_flag(),
                self.cur_display_gen,
                None,
                Some(resize_trace.as_ref()),
                self.client_hdr,
                self.clock.au_seq,
            ) {
                Ok(next_pipe) => {
                    built_bitrate = next_pipe.bitrate_kbps;
                    self.swap_pipeline(Next::Pipeline(next_pipe), SwapReason::ModeSwitch);
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
        self.adopt_built_bitrate(built_bitrate);
        self.cur_mode = new_mode;
        self.next = std::time::Instant::now();
        self.publish_delivered_mode(new_mode);
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
                self.life.quit_flag(),
                self.cur_display_gen,
                None,
                Some(trace.as_ref()),
                self.client_hdr,
                self.clock.au_seq,
            ) {
                Ok(next_pipe) => {
                    let built = next_pipe.bitrate_kbps;
                    self.swap_pipeline(Next::Pipeline(next_pipe), SwapReason::TopologyReassert);
                    self.adopt_built_bitrate(built);
                }
                Err(e) => {
                    return Err(e).context(
                        "exclusive-topology eviction recovery failed, and the full \
                         pipeline rebuild after it failed too",
                    );
                }
            }
        }
        trace.finish("pipeline_rebuilt");
        announce_pipeline_gap(&self.gap_tx, trace.total_slot().load(Ordering::Relaxed));
        Ok(())
    }

    /// Capture failed. On Linux a dedicated game session whose game exited emits `game.exited`
    /// and ends cleanly (`Ok(false)`); otherwise [`rebuild_after_capture_loss`], re-detecting
    /// the live compositor each attempt. `Err` = the rebuild budget or the rebuild count is
    /// exhausted.
    pub(super) fn on_capture_lost(&mut self, e: anyhow::Error) -> Result<bool> {
        #[cfg(not(target_os = "linux"))]
        let _ = (&self.cur_node_id, &self.game_life);
        #[cfg(target_os = "linux")]
        if self.vd_params.launch.is_some()
            && crate::session_settings::get().session_on_game_exit
            && crate::vdisplay::launch_is_nested(self.compositor, self.vd_params.route.as_ref())
            && crate::vdisplay::dedicated_game_exited(self.cur_node_id)
        {
            tracing::info!("dedicated game session: the game exited — ending the session cleanly");
            if let Some(g) = self.game_life.as_ref() {
                crate::gamelease::report_exit(&g.shared());
            }
            self.life.end(crate::events::SessionEndReason::GameExited);
            self.conn
                .close(punktfunk_core::quic::APP_EXITED_CLOSE_CODE, b"game exited");
            return Ok(false);
        }
        let keepalive = count_capture_loss(
            self.conn.plane(),
            &mut self.capture_rebuilds,
            e,
            &mut *self.capturer,
        )?;
        // The QUIC keepalive holds the connection: the client stays frozen on the last frame
        // and resumes, no reconnect.
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
        let pipe = rebuild_after_capture_loss(self, keepalive, &budget)?;
        let built = pipe.bitrate_kbps;
        self.swap_pipeline(Next::Pipeline(pipe), SwapReason::CaptureLost);
        #[cfg(target_os = "linux")]
        {
            self.no_overlay_means_off_output =
                settle_portal_cursor(&*self.vd, &mut self.metadata_composite);
        }
        self.adopt_built_bitrate(built);
        self.enc.request_keyframe();
        self.next = std::time::Instant::now();
        tracing::info!(
            compositor = self.compositor.id(),
            "capture loss: pipeline rebuilt — stream resumes"
        );
        Ok(true)
    }

    /// The request a rebuild opens a display with: the session's, at `hw_cursor` and `route` of
    /// the compositor it lands on. `launch` is `None`: a rebuild never starts the game again.
    fn reopen_params(
        &self,
        hw_cursor: bool,
        route: Option<crate::vdisplay::GamescopeRoute>,
    ) -> crate::vdisplay::SessionParams {
        crate::vdisplay::SessionParams {
            hw_cursor,
            route,
            launch: None,
            ..self.vd_params.clone()
        }
    }

    /// One capture-loss attempt's re-detection: follow the live session's compositor, opening a
    /// new backend with the session's request when it changed, re-point input at it and
    /// re-derive the cursor plan.
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
        let mut reopened = false;
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
                    reopened = true;
                }
                Err(e2) => tracing::warn!(error = %format!("{e2:#}"),
                    "capture loss: opening the newly-detected compositor failed — retrying"),
            }
        }
        // Also when only the gamescope route changed: Attach and Spawn differ in who draws.
        let hw_cursor = self.retarget_cursor_plan(self.compositor, rebuilt_route.as_ref());
        if reopened {
            self.reopen_params(hw_cursor, rebuilt_route)
                .apply(&mut *self.vd);
        } else {
            // The live display keeps the rest of its request, launch included.
            self.vd.set_hw_cursor(hw_cursor);
            self.vd.set_gamescope_route(rebuilt_route);
        }
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
            self.clock.au_seq,
        )
        .with_context(|| {
            format!(
                "the capture source changed to {}x{} {:?} mid-session and the encoder could not \
                 be reopened at it",
                self.frame.width, self.frame.height, self.frame.format
            )
        });
        let (enc, reframe) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                let Some(backoff) = self.clock.watchdog.spend(self.interval) else {
                    return Err(e).context("encoder reopen at the source's new mode");
                };
                tracing::warn!(error = %format!("{e:#}"), reset = self.clock.watchdog.resets(),
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
        let next = Next::Encoder {
            enc,
            reframe,
            resized: None,
        };
        self.swap_pipeline(next, SwapReason::SourceMode);
        // The delivered mode is the session's now: a rebuild or topology re-assert
        // reopens at it instead of forcing the display back to the client's ask.
        self.cur_mode = actual;
        self.adopt_built_bitrate(src_kbps);
        self.live_mode.store(
            pack_mode(actual.width, actual.height, actual.refresh_hz),
            Ordering::Relaxed,
        );
        self.tell_delivered(actual, true);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Plane;
    use crate::vdisplay::Compositor;
    use std::collections::VecDeque;

    /// A capturer whose output is still up: it holds a keepalive until asked for it.
    struct Lost(Option<Box<dyn Send>>);

    impl crate::capture::Capturer for Lost {
        fn next_frame(&mut self) -> Result<crate::capture::CapturedFrame> {
            anyhow::bail!("lost")
        }
        fn take_keepalive(&mut self) -> Option<Box<dyn Send>> {
            self.0.take()
        }
    }

    /// One loop's arguments: the native loop judges the budget on the compositor it runs on,
    /// GameStream on the one live at the loss.
    struct Fake {
        plane: Plane,
        compositor: Option<Compositor>,
        reattaches: bool,
        builds: VecDeque<Result<&'static str>>,
        ending: bool,
        calls: Vec<&'static str>,
    }

    impl CaptureLossRebuild for Fake {
        type Built = &'static str;
        fn plane(&self) -> Plane {
            self.plane
        }
        fn reattach(&mut self, _keepalive: Box<dyn Send>) -> Option<&'static str> {
            self.calls.push("reattach");
            self.reattaches.then_some("reattached")
        }
        fn build(&mut self) -> Result<&'static str> {
            self.calls.push("build");
            self.builds.pop_front().expect("a scripted build")
        }
        fn ending(&self) -> bool {
            self.ending
        }
        fn budget_compositor(&self) -> Option<Compositor> {
            self.compositor
        }
    }

    fn loops() -> [Fake; 2] {
        let fake = |plane, compositor| Fake {
            plane,
            compositor,
            reattaches: false,
            builds: VecDeque::new(),
            ending: false,
            calls: Vec::new(),
        };
        [
            fake(Plane::Native, Some(Compositor::Kwin)),
            fake(Plane::Gamestream, None),
        ]
    }

    #[test]
    fn a_capture_loss_spends_the_rebuild_count_then_fails() {
        for l in loops() {
            let mut rebuilds = 0;
            for _ in 0..MAX_CAPTURE_REBUILDS {
                let lost = anyhow::anyhow!("stream error");
                let kept = count_capture_loss(l.plane, &mut rebuilds, lost, &mut Lost(None));
                assert!(kept.expect("within the count").is_none());
            }
            let lost = anyhow::anyhow!("stream error");
            let e = count_capture_loss(l.plane, &mut rebuilds, lost, &mut Lost(None))
                .err()
                .expect("past the count");
            assert!(format!("{e:#}").contains("rebuild attempts exhausted"));
        }
    }

    #[test]
    fn only_a_display_still_alive_hands_back_the_keepalive() {
        for l in loops() {
            let mut rebuilds = 0;
            let alive = anyhow::anyhow!("import failed").context(pf_capture::DisplayStillAlive);
            let mut cap = Lost(Some(Box::new(())));
            let kept = count_capture_loss(l.plane, &mut rebuilds, alive, &mut cap).unwrap();
            assert!(kept.is_some());
            let mut cap = Lost(Some(Box::new(())));
            let gone = anyhow::anyhow!("compositor went away");
            let kept = count_capture_loss(l.plane, &mut rebuilds, gone, &mut cap).unwrap();
            assert!(kept.is_none());
            assert!(cap.0.is_some(), "a dead display keeps its keepalive");
        }
    }

    #[test]
    fn a_rebuild_reattaches_first_then_builds_until_one_comes_up() {
        let budget = RebuildBudget::start();
        for mut l in loops() {
            l.reattaches = true;
            let got = rebuild_after_capture_loss(&mut l, Some(Box::new(())), &budget).unwrap();
            assert_eq!((got, &l.calls[..]), ("reattached", &["reattach"][..]));
        }
        for mut l in loops() {
            l.builds = [Err(anyhow::anyhow!("not up yet")), Ok("built")].into();
            let got = rebuild_after_capture_loss(&mut l, Some(Box::new(())), &budget).unwrap();
            assert_eq!(got, "built");
            assert_eq!(l.calls, ["reattach", "build", "build"]);
        }
        for mut l in loops() {
            l.builds = [Ok("built")].into();
            rebuild_after_capture_loss(&mut l, None, &budget).unwrap();
            assert_eq!(l.calls, ["build"], "no keepalive, no re-attach");
        }
    }

    #[test]
    fn an_ending_session_stops_retrying() {
        let budget = RebuildBudget::start();
        for mut l in loops() {
            l.ending = true;
            l.builds = [Err(anyhow::anyhow!("not up yet"))].into();
            let e = rebuild_after_capture_loss(&mut l, None, &budget)
                .expect_err("an ending session gives up");
            assert!(format!("{e:#}").contains("within the rebuild budget"));
            assert_eq!(l.calls, ["build"]);
        }
    }
}
