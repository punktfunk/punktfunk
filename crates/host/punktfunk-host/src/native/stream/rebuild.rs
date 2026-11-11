//! Everything that swaps the pipeline under a running stream: a Gaming↔Desktop session switch,
//! a client mode switch, a codec ask, the Windows topology re-assert, capture loss, a source
//! that changed size with no client Reconfigure, and a rate the encoder cannot take in place.
//! Each site drains its ask into [`StreamState::rebuild`], the one performer: it plans the
//! rebuild ([`rebuild_plan::plan`]), prepares, builds, then commits through
//! [`StreamState::swap_pipeline`] and the plan's epilogue, or rolls back.

#[cfg(target_os = "linux")]
use super::cursor::settle_portal_cursor;
#[cfg(target_os = "linux")]
use super::pipeline::reattach_pipeline;
use super::pipeline::{
    build_pipeline, build_pipeline_with_retry, is_permanent_build_error, open_session_encoder_with,
    rate_at_encoded, BuildRequest, Pipeline,
};
use super::rebuild_plan::{
    self, Build, DisplayChange, Rate, RebuildAsk, RebuildFacts, RebuildPlan, Screens, Tell,
};
use super::state::{announce_gap_since, announce_pipeline_gap, StreamState};
use super::swap::Next;
use super::*;
use crate::bringup::Trace;
use crate::encode_recovery::{
    EncoderWatchdog, RebuildBudget, MAX_CAPTURE_REBUILDS, MAX_ENCODER_RESETS,
};
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
        .and_then(|_| capturer.feed().and_then(|f| f.take_keepalive())))
}

/// One stream loop's side of [`rebuild_after_capture_loss`].
pub(crate) trait CaptureLossRebuild {
    /// What a rebuild hands the loop.
    type Built;
    fn plane(&self) -> crate::events::Plane;
    /// Capture again on the output the loss left up. `None` = it could not; the loop logs why.
    fn reattach(&mut self, keepalive: Box<dyn Send>) -> Option<Self::Built>;
    /// One attempt at a new source. `first` is the loss's first pass: only it may take a
    /// short first-frame wait, so a producer that needs longer can come up on a later one.
    fn build(&mut self, first: bool) -> Result<Self::Built>;
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
    let mut first = true;
    loop {
        let _probe = budget.probe_scope();
        match l.build(std::mem::take(&mut first)) {
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

/// A source's pixel format and size.
pub(crate) type SourceMode = (pf_frame::PixelFormat, u32, u32);

/// What [`follow_source_mode`] did this tick.
pub(crate) enum SourceFollow<T> {
    /// The source still runs at the encoder's mode.
    Unchanged,
    /// The encoder reopened at the source's mode; the loop adopts what `reopen` built.
    Reopened(T),
    /// The reopen failed and the tick went on the backoff.
    BackedOff,
}

/// The source moved `from` the encoder's mode `to` another with nothing negotiating it:
/// `reopen` the encoder at it. A failed reopen spends the shared reset budget, moves the
/// loop's next tick past the backoff and sleeps through it; `Err` once the budget is spent.
/// `retry` lends the loop's watchdog and next-tick clock.
pub(crate) fn follow_source_mode<S, T>(
    s: &mut S,
    from: SourceMode,
    to: SourceMode,
    interval: std::time::Duration,
    reopen: impl FnOnce(&mut S) -> Result<T>,
    retry: impl FnOnce(&mut S) -> (&mut EncoderWatchdog, &mut std::time::Instant),
) -> Result<SourceFollow<T>> {
    if from == to {
        return Ok(SourceFollow::Unchanged);
    }
    let e = match reopen(s) {
        Ok(t) => return Ok(SourceFollow::Reopened(t)),
        Err(e) => e,
    };
    let (watchdog, next) = retry(s);
    // A first failed open is a settling driver.
    let Some(backoff) = watchdog.spend(interval) else {
        return Err(e).context("encoder reopen at the source's new mode");
    };
    tracing::warn!(error = %format!("{e:#}"), reset = watchdog.resets(),
        max = MAX_ENCODER_RESETS,
        "reopening the encoder at the source's new mode failed — retrying");
    *next = std::time::Instant::now() + backoff;
    std::thread::sleep(backoff);
    Ok(SourceFollow::BackedOff)
}

impl CaptureLossRebuild for StreamState {
    type Built = Pipeline;

    fn plane(&self) -> crate::events::Plane {
        self.facts.conn.plane()
    }

    /// Re-attach before creating another display: on KWin a create is a new virtual output, and
    /// a burst of them wedges the compositor into placeholder screens.
    #[cfg(target_os = "linux")]
    fn reattach(&mut self, keepalive: Box<dyn Send>) -> Option<Pipeline> {
        let lease = self.live.lease.clone()?;
        let req = self.build_request(self.live.cur_mode, self.live.bitrate_kbps);
        match reattach_pipeline(&mut self.display.vd, lease, keepalive, &req) {
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
    fn build(&mut self, first: bool) -> Result<Pipeline> {
        if pf_host_config::config().compositor.is_none() {
            self.retarget_to_live_session();
        }
        let req = self.build_request(self.live.cur_mode, self.live.bitrate_kbps);
        build_pipeline_with_retry(
            &mut self.display.vd,
            &req,
            self.facts.life.quit_flag(),
            self.facts.life.stop_flag(),
            self.live.cur_display_gen,
            // A later pass is attempt 2: the capturer's own first-frame wait, not the short one.
            if first { 1..=1 } else { 2..=2 },
            None,
        )
    }

    fn ending(&self) -> bool {
        self.facts.life.stopping()
    }

    /// The compositor this session runs on now: each attempt's re-detection can move it.
    fn budget_compositor(&self) -> Option<crate::vdisplay::Compositor> {
        Some(self.display.compositor)
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

/// A pipeline on `vd` the way `build` says: one build, or bring-up's retry over its attempts.
/// `supersedes` is the display generation it replaces.
fn build_on(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    req: &BuildRequest,
    life: &crate::lifecycle::Lifecycle,
    supersedes: Option<u64>,
    build: &Build,
    trace: Option<&crate::bringup::Trace>,
) -> Result<Pipeline> {
    let quit = life.quit_flag();
    match build {
        Build::Retry(attempts) => build_pipeline_with_retry(
            vd,
            req,
            quit,
            life.stop_flag(),
            supersedes,
            attempts.clone(),
            trace,
        ),
        _ => build_pipeline(vd, req, quit, supersedes, None, trace),
    }
}

/// What a build hands the commit.
struct Built {
    next: Next,
    /// The rate the build opened at, kbps.
    rate_kbps: u32,
    /// The display a session switch opened; it replaces the live one.
    vd: Option<Box<dyn crate::vdisplay::VirtualDisplay>>,
}

impl Built {
    fn pipeline(pipe: Pipeline, vd: Option<Box<dyn crate::vdisplay::VirtualDisplay>>) -> Self {
        Built {
            rate_kbps: pipe.bitrate_kbps,
            next: Next::Pipeline(pipe),
            vd,
        }
    }
}

/// What became of an ask.
pub(super) enum Rebuilt {
    /// Nothing needed rebuilding.
    Unchanged,
    Built,
    /// The build failed; the stream runs what it ran.
    Kept(anyhow::Error),
}

/// What the session ran before a rebuild changed it: a failed build restores it, the commit
/// compares against it.
struct Before {
    plan: crate::session_plan::SessionPlan,
    bit_depth: u8,
    composite: (bool, bool),
    params: crate::vdisplay::SessionParams,
    compositor: crate::vdisplay::Compositor,
}

/// A rebuild between prepare and commit.
struct Prepared {
    before: Before,
    /// The request the session's displays open with once this commits.
    params: Option<crate::vdisplay::SessionParams>,
    /// The live display was handed the new request's HDR and cursor.
    shown: bool,
    /// The session switch's launch, for the display it opens.
    launch: Option<String>,
    /// The lost capturer's keepalive, for the re-attach.
    keepalive: Option<Box<dyn Send>>,
}

/// The one rebuild performer: [`StreamState::rebuild`] and the steps it runs.
impl StreamState {
    /// What [`rebuild_plan::plan`] reads.
    fn rebuild_facts(&self) -> RebuildFacts {
        RebuildFacts {
            mode: self.live.cur_mode,
            rate_kbps: self.live.bitrate_kbps,
            plan: self.plan,
            compositor: self.display.compositor,
            bitrate_auto: self.facts.bitrate_auto,
            pyrowave_max_kbps: self.facts.knobs.pyrowave_max_kbps,
            seen_reassert_gen: self.seen_reassert_gen,
            follows_session: pf_host_config::config().compositor.is_none(),
            windows: cfg!(target_os = "windows"),
        }
    }

    /// Plan `ask` and perform it: prepare (snapshot what it changes, apply the display change),
    /// build, then commit or roll back. `Err` only where the plan ends the session on a failed
    /// build. The live mode asked again still tells the control task what is delivered (S5-6).
    pub(super) fn rebuild(&mut self, ask: RebuildAsk) -> Result<Rebuilt> {
        let Some(plan) = rebuild_plan::plan(&ask, &self.rebuild_facts()) else {
            if let RebuildAsk::Mode(asked, _) = ask {
                self.publish_delivered_mode(asked);
            }
            return Ok(Rebuilt::Unchanged);
        };
        self.perform(plan, ask, Self::build)
    }

    /// [`Self::rebuild`] past its plan, with `build` the step a test scripts.
    fn perform(
        &mut self,
        plan: RebuildPlan,
        ask: RebuildAsk,
        build: impl FnOnce(&mut Self, &RebuildPlan, &mut Prepared, Option<&Trace>) -> Result<Built>,
    ) -> Result<Rebuilt> {
        let t0 = std::time::Instant::now();
        let trace = plan
            .trace
            .map(|kind| Trace::start(kind, self.shared.resize_ms.clone()));
        let mut prep = self.prepare(&plan, ask);
        match build(self, &plan, &mut prep, trace.as_deref()) {
            Ok(built) => {
                self.commit(&plan, built, prep, t0, trace.as_deref());
                Ok(Rebuilt::Built)
            }
            Err(e) => {
                self.roll_back(&plan, prep, t0);
                if plan.rollback.ends {
                    Err(e)
                } else {
                    Ok(Rebuilt::Kept(e))
                }
            }
        }
    }

    /// Snapshot what the plan changes, take the ask's payload — a mode ask's view goes into the
    /// session plan — and apply its display change: a switch's env, route and cursor plan, or a
    /// codec's.
    fn prepare(&mut self, plan: &RebuildPlan, ask: RebuildAsk) -> Prepared {
        let d = &self.display;
        let mut prep = Prepared {
            before: Before {
                plan: self.plan,
                bit_depth: self.bit_depth,
                composite: (d.gamescope_composite, d.metadata_composite),
                params: d.vd_params.clone(),
                compositor: d.compositor,
            },
            params: None,
            shown: false,
            launch: None,
            keepalive: None,
        };
        match ask {
            RebuildAsk::SessionSwitch(sw) => {
                tracing::info!(from = self.display.compositor.id(), to = sw.compositor.id(),
                    kind = ?sw.kind, "session switch — rebuilding backend in place");
                // Being the only writer proves nothing: `setenv` races every concurrent `getenv`.
                if let Some(env) = sw.env {
                    crate::vdisplay::apply_session_env(&crate::vdisplay::ActiveSession {
                        kind: sw.kind,
                        env,
                        compositor_pid: None,
                    });
                }
                prep.launch = sw.launch;
            }
            RebuildAsk::Mode(new_mode, view) => {
                tracing::info!(?new_mode, ?view, "rebuilding pipeline for mode switch");
                self.plan.reframe_to = view.map(|v| (punktfunk_core::video_fit::VideoFit::Fit, v));
            }
            RebuildAsk::TopologyReassert(_) => tracing::info!(
                "exclusive-topology eviction bounced the virtual display's modes — rebuilding \
                 the capture attachment in place at the current mode"
            ),
            RebuildAsk::CaptureLost(keepalive) => prep.keepalive = keepalive,
            RebuildAsk::Format(_) | RebuildAsk::SourceMode(_) | RebuildAsk::Bitrate(_) => {}
        }

        match plan.display {
            Some(DisplayChange::Switch(to)) => {
                let route = crate::vdisplay::resolve_gamescope_route(to, false);
                if matches!(
                    to,
                    crate::vdisplay::Compositor::Kwin | crate::vdisplay::Compositor::Mutter
                ) {
                    crate::vdisplay::settle_desktop_portal(to);
                }
                let hw_cursor = self.retarget_cursor_plan(to, route.as_ref());
                prep.params = Some(crate::vdisplay::SessionParams {
                    hw_cursor,
                    route,
                    ..self.display.vd_params.clone()
                });
            }
            Some(DisplayChange::Format(codec)) => self.reformat(codec, plan.mode, &mut prep),
            None => {}
        }
        prep
    }

    /// Re-run the handshake's depth, HDR and chroma rule for `codec` at `mode` (S2-5), resolve
    /// the plan for it, and hand the live display its HDR verdict and cursor plan (S5-4).
    fn reformat(
        &mut self,
        codec: crate::encode::Codec,
        mode: punktfunk_core::Mode,
        prep: &mut Prepared,
    ) {
        let (bit_depth, hdr, chroma) =
            self.facts
                .format_facts
                .resolve(codec, &self.facts.encoder, mode);
        self.plan = super::pipeline::resolve_plan(
            &self.facts.encoder,
            bit_depth,
            hdr,
            chroma,
            codec,
            self.plan.studio_asked,
            self.plan.cursor_forward,
            self.plan.slices,
            self.display.compositor,
            self.display.vd_params.route.as_ref(),
            self.plan.pyrowave_bpp,
            usize::from(self.facts.shard_payload),
            self.plan.reframe_to,
            self.display.vd_params.join_live,
            self.plan.mirrored,
        );
        self.bit_depth = bit_depth;
        let route = self.display.vd_params.route.clone();
        let hw_cursor = self.retarget_cursor_plan(self.display.compositor, route.as_ref());
        self.display.vd.set_hdr(hdr);
        self.display.vd.set_hw_cursor(hw_cursor);
        prep.shown = true;
        prep.params = Some(crate::vdisplay::SessionParams {
            hdr,
            hw_cursor,
            ..self.display.vd_params.clone()
        });
        tracing::info!(from = ?prep.before.plan.codec, to = ?codec, bit_depth, hdr, ?chroma,
            "rebuilding the pipeline for a codec change");
    }

    /// The plan's build: in place first where it says, else its pipeline or encoder.
    fn build(
        &mut self,
        plan: &RebuildPlan,
        prep: &mut Prepared,
        trace: Option<&Trace>,
    ) -> Result<Built> {
        if let (true, Some(t)) = (plan.in_place, trace) {
            if let Some((next, rate_kbps)) = self.in_place(plan, t) {
                return Ok(Built {
                    next,
                    rate_kbps,
                    vd: None,
                });
            }
        }
        match (&plan.build, plan.display) {
            (Build::Encoder, _) => self.build_encoder(plan, crate::encode::open_video),
            (Build::CaptureLoss, _) => {
                let pipe = self.build_after_loss(prep.keepalive.take())?;
                Ok(Built::pipeline(pipe, None))
            }
            (build, Some(DisplayChange::Switch(to))) => {
                let client = prep.params.as_ref().unwrap_or(&self.display.vd_params);
                let mut vd = crate::vdisplay::open_for(to, client.client_fp)?;
                let params = crate::vdisplay::SessionParams {
                    launch: prep.launch.take(),
                    ..prep
                        .params
                        .clone()
                        .unwrap_or_else(|| self.display.vd_params.clone())
                };
                params.apply(&mut *vd);
                let req = self.build_request(plan.mode, plan.rate_kbps);
                let pipe = build_on(&mut vd, &req, &self.facts.life, None, build, trace)?;
                Ok(Built::pipeline(pipe, Some(vd)))
            }
            (build, _) => {
                let req = self.build_request(plan.mode, plan.rate_kbps);
                let live_gen = self.live.cur_display_gen;
                let vd = &mut self.display.vd;
                let pipe = build_on(vd, &req, &self.facts.life, live_gen, build, trace)?;
                Ok(Built::pipeline(pipe, None))
            }
        }
    }

    /// The encoder alone, on the live capture at the delivered refresh, through `open_video`, at
    /// the plan's rate. A rate the plan priced follows the size the encoder opened at
    /// ([`rate_at_encoded`]), as a full build's does; an asked one stands.
    fn build_encoder(
        &mut self,
        plan: &RebuildPlan,
        open_video: impl FnMut(
            &crate::encode::Resolved,
            crate::encode::OpenParams,
        ) -> Result<Box<dyn crate::encode::Encoder>>,
    ) -> Result<Built> {
        let ed = self.enc_now();
        let wire_seq_base = self.clock.next_au();
        let priced = self.plan.picture(plan.mode);
        let (auto, cap) = (self.facts.bitrate_auto, self.facts.knobs.pyrowave_max_kbps);
        let kbps_for = |w, h| match plan.epilogue.rate {
            Rate::Built => rate_at_encoded(plan.rate_kbps, auto, priced, (w, h), &self.plan, cap),
            Rate::Applied => plan.rate_kbps,
        };
        let (enc, reframe) = open_session_encoder_with(
            &self.facts.encoder,
            &self.plan,
            &*self.live.capturer,
            &self.live.frame,
            (self.facts.negotiated.width, self.facts.negotiated.height),
            self.delivered_mode().refresh_hz,
            |w, h| ed.enc_kbps(kbps_for(w, h)) as u64 * 1000,
            self.bit_depth,
            self.facts.client_hdr,
            wire_seq_base,
            open_video,
        )?;
        let rate_kbps = kbps_for(reframe.out.0, reframe.out.1);
        let next = Next::Encoder {
            enc,
            reframe,
            resized: None,
        };
        Ok(Built {
            next,
            rate_kbps,
            vd: None,
        })
    }

    /// [`rebuild_after_capture_loss`] within a fresh budget. The QUIC keepalive holds the
    /// connection meanwhile: the client stays frozen on the last frame and resumes.
    fn build_after_loss(&mut self, keepalive: Option<Box<dyn Send>>) -> Result<Pipeline> {
        let budget = RebuildBudget::start();
        if pf_host_config::config().compositor.is_some() {
            let active = crate::vdisplay::detect_active_session();
            if crate::vdisplay::compositor_for_kind(active.kind) != Some(self.display.compositor) {
                tracing::warn!(
                    pinned = self.display.compositor.id(),
                    live = ?active.kind,
                    "capture lost while PUNKTFUNK_COMPOSITOR pins the backend and the \
                     live session no longer matches it — the pin disables \
                     session-following, so this rebuild can only retry the pinned \
                     backend; remove the pin to let the stream follow session switches"
                );
            }
        }
        rebuild_after_capture_loss(self, keepalive, &budget)
    }

    /// The Windows in-place mode set ([`Self::resize`]) for `plan`, and the rate its encoder
    /// opened at, priced at the picture it encodes as [`Self::build_encoder`] prices it. `None` =
    /// the full build runs.
    #[cfg(target_os = "windows")]
    fn in_place(&mut self, plan: &RebuildPlan, trace: &Trace) -> Option<(Next, u32)> {
        let recover_ring = plan.reason == super::swap::SwapReason::TopologyReassert;
        let (session, priced) = (self.plan, self.plan.picture(plan.mode));
        let (auto, cap) = (self.facts.bitrate_auto, self.facts.knobs.pyrowave_max_kbps);
        let kbps_for = move |w, h| match plan.epilogue.rate {
            Rate::Built => rate_at_encoded(plan.rate_kbps, auto, priced, (w, h), &session, cap),
            Rate::Applied => plan.rate_kbps,
        };
        let next = self.resize(plan.mode, kbps_for, trace, recover_ring)?;
        let rate_kbps = match &next {
            Next::Encoder { reframe, .. } => kbps_for(reframe.out.0, reframe.out.1),
            _ => plan.rate_kbps,
        };
        Some((next, rate_kbps))
    }

    /// Only a Windows IDD-push display is mode-set in place.
    #[cfg(not(target_os = "windows"))]
    fn in_place(&mut self, _plan: &RebuildPlan, _trace: &Trace) -> Option<(Next, u32)> {
        None
    }

    /// Swap the build in, then the plan's epilogue: the display request it commits to and the
    /// colour later joiners read off it, the input route, the rate, the further screens, the
    /// mode and who hears it, the gap.
    fn commit(
        &mut self,
        plan: &RebuildPlan,
        built: Built,
        prep: Prepared,
        t0: std::time::Instant,
        trace: Option<&Trace>,
    ) {
        let e = plan.epilogue;
        // The open frame is the old encoder's until the new one takes over: a failed build keeps
        // both, and the new encoder numbered its first AU past it.
        self.close_open_frame();
        self.swap_pipeline(built.next, plan.reason);
        if let Some(vd) = built.vd {
            self.display.vd = vd;
        }
        if let Some(DisplayChange::Switch(to)) = plan.display {
            self.display.compositor = to;
        }
        // The request every later display of this session starts from.
        if let Some(params) = prep.params {
            crate::vdisplay::admission::publish_hdr(self.facts.life.stop_flag(), params.hdr);
            self.display.vd_params = params;
        }
        if e.input {
            self.repoint_input();
        }
        match e.rate {
            Rate::Built => self.adopt_built_bitrate(built.rate_kbps),
            Rate::Applied => self.adopt_rebuilt_rate(plan.rate_kbps),
        }
        let moved = self.display.compositor != prep.before.compositor;
        if e.screens == Screens::Rebase || (e.screens == Screens::RebaseIfMoved && moved) {
            self.rebase_screens();
        }
        if e.portal_cursor {
            #[cfg(target_os = "linux")]
            {
                self.display.no_overlay_means_off_output =
                    settle_portal_cursor(&*self.display.vd, &mut self.display.metadata_composite);
            }
        }
        self.live.cur_mode = plan.mode;
        if e.keyframe {
            self.live.enc.request_keyframe();
        }
        // The pacing clock restarts: the stall was the rebuild's, not encode falling behind.
        self.next = std::time::Instant::now();
        match (e.tell, plan.display) {
            (Some(Tell::Mode), _) => self.publish_delivered_mode(plan.mode),
            (Some(Tell::Correction), _) => {
                let actual = self.delivered_mode();
                let packed = pack_mode(actual.width, actual.height, actual.refresh_hz);
                self.shared.live_mode.store(packed, Ordering::Relaxed);
                self.tell_delivered(actual, true);
            }
            (Some(Tell::Codec), Some(DisplayChange::Format(codec))) => {
                let _ = self.ends.format_result_tx.send(Some(codec));
            }
            _ => {}
        }
        // Reconfigured clears baselines, not the straddling window or slow start.
        match trace {
            Some(t) => {
                t.finish("pipeline_rebuilt");
                let total = t.total_slot().load(Ordering::Relaxed);
                announce_pipeline_gap(&self.ends.gap_tx, total);
            }
            None => announce_gap_since(&self.ends.gap_tx, t0),
        }
        // Capture rebuilt at the mode, which is what the eviction check would do: a topology
        // bump this rebuild caused is not somebody else's.
        if e.reassert_sync {
            self.seen_reassert_gen = crate::windows::idd::topology_reassert_gen();
        }
    }

    /// Input follows the stream onto the compositor it runs on, with that compositor's route.
    fn repoint_input(&self) {
        #[cfg(target_os = "linux")]
        repoint_session_input(
            &self.ends.input_route,
            &self.ends.inj_shared_tx,
            self.ends.inj_session_tx.as_ref(),
            self.display.compositor,
            self.display.vd_params.route.as_ref(),
        );
        #[cfg(not(target_os = "linux"))]
        crate::inject::set_backend_id(crate::vdisplay::input_backend_id(self.display.compositor));
    }

    /// Put back what prepare changed, then announce the stall and tell what still runs, as the
    /// plan's rollback says.
    fn roll_back(&mut self, plan: &RebuildPlan, prep: Prepared, t0: std::time::Instant) {
        let Before {
            plan: old_plan,
            bit_depth,
            composite,
            params,
            ..
        } = prep.before;
        (self.plan, self.bit_depth) = (old_plan, bit_depth);
        let d = &mut self.display;
        (d.gamescope_composite, d.metadata_composite) = composite;
        if prep.shown {
            d.vd.set_hdr(params.hdr);
            d.vd.set_hw_cursor(params.hw_cursor);
        }
        d.vd_params = params;
        if plan.rollback.gap {
            announce_gap_since(&self.ends.gap_tx, t0);
        }
        match plan.rollback.tell {
            Some(Tell::Mode) => self.tell_delivered(self.delivered_mode(), true),
            Some(Tell::Codec) => {
                let _ = self.ends.format_result_tx.send(None);
            }
            Some(Tell::Rate) => self.keep_rate(plan.rate_kbps),
            Some(Tell::Correction) | None => {}
        }
    }
}

/// The seven rebuild sites: each drains its ask and hands it to [`StreamState::rebuild`].
impl StreamState {
    /// Follow the latest session switch, the watcher's or the seat's: rebuild the backend in
    /// place, with the session's display request, the switch's launch and the cursor plan of the
    /// compositor it switches to, and keep streaming. A launched game the old session takes
    /// down with it does not end the stream.
    pub(super) fn on_session_switch(&mut self) {
        let Some(sw) = self.ends.session_rx.try_iter().last() else {
            return;
        };
        // The box's sessions end on a switch, and so does a seat's Game Mode; a seat's desktop
        // runs on under its Game Mode.
        let leaves_its_apps =
            sw.env.is_some() || self.display.compositor == crate::vdisplay::Compositor::Gamescope;
        if let Some(g) = self.game_life.as_ref().filter(|_| leaves_its_apps) {
            g.shared().end_with_session();
        }
        match self.rebuild(RebuildAsk::SessionSwitch(sw)) {
            Ok(Rebuilt::Built) => tracing::info!(
                compositor = self.display.compositor.id(),
                "session switch — backend rebuilt, stream continues"
            ),
            Ok(Rebuilt::Kept(e)) | Err(e) => {
                let chain = format!("{e:#}");
                let kind = if is_permanent_build_error(&chain) {
                    "permanent"
                } else {
                    "transient"
                };
                tracing::warn!(error = %chain, kind,
                    "session-switch rebuild failed — staying on the current backend");
            }
            Ok(Rebuilt::Unchanged) => {}
        }
    }

    /// The client's latest accepted mode and view: in place on Windows IDD-push, else a full
    /// rebuild; a new view alone reopens the encoder; the mode and view the session already runs
    /// rebuild nothing. A failed rebuild stays on what runs and tells the client so. PyroWave
    /// re-resolves its pin for the new picture.
    pub(super) fn on_mode_switch(&mut self) {
        let Some((new_mode, view)) = self.ends.reconfig.try_iter().last() else {
            return;
        };
        if let Ok(Rebuilt::Kept(e)) | Err(e) = self.rebuild(RebuildAsk::Mode(new_mode, view)) {
            tracing::warn!(error = %format!("{e:#}"), ?new_mode,
                "mode-switch rebuild failed — staying on the current mode");
        }
    }

    /// The client's codec ask, vetted by the control task: rebuild at the current mode with the
    /// depth, HDR and chroma the handshake's rule gives the new codec. The control task acks
    /// from the outcome; a failed rebuild keeps the old plan and pipeline.
    pub(super) fn on_format_change(&mut self) {
        let Some(codec) = self.ends.format_rx.try_iter().last() else {
            return;
        };
        if let Ok(Rebuilt::Kept(e)) | Err(e) = self.rebuild(RebuildAsk::Format(codec)) {
            tracing::warn!(error = %format!("{e:#}"), ?codec,
                "codec-change rebuild failed — staying on the current codec");
        }
    }

    /// An exclusive-topology eviction bounced the Windows IDD-push display's modes: re-attach
    /// capture in place at the current mode, or rebuild the pipeline outright. Ending the session
    /// on an idle desktop would just loop the client.
    pub(super) fn on_topology_reassert(&mut self) -> Result<()> {
        let generation = crate::windows::idd::topology_reassert_gen();
        self.rebuild(RebuildAsk::TopologyReassert(generation))
            .context(
                "exclusive-topology eviction recovery failed, and the full pipeline rebuild \
                 after it failed too",
            )
            .map(drop)
    }

    /// Capture failed. On Linux a dedicated game session whose game exited emits `game.exited`
    /// and ends cleanly (`Ok(false)`); otherwise [`rebuild_after_capture_loss`], re-detecting
    /// the live compositor each attempt. `Err` = the rebuild budget or the rebuild count is
    /// exhausted.
    pub(super) fn on_capture_lost(&mut self, e: anyhow::Error) -> Result<bool> {
        if self.end_on_game_exit() {
            return Ok(false);
        }
        let keepalive = count_capture_loss(
            self.facts.conn.plane(),
            &mut self.capture_rebuilds,
            e,
            &mut *self.live.capturer,
        )?;
        self.rebuild(RebuildAsk::CaptureLost(keepalive))?;
        tracing::info!(
            compositor = self.display.compositor.id(),
            "capture loss: pipeline rebuilt — stream resumes"
        );
        Ok(true)
    }

    /// A dedicated game session whose game exited: report it and end the session cleanly.
    #[cfg(target_os = "linux")]
    fn end_on_game_exit(&mut self) -> bool {
        let exited = self.display.vd_params.launch.is_some()
            && crate::session_settings::get().session_on_game_exit
            && crate::vdisplay::launch_is_nested(
                self.display.compositor,
                self.display.vd_params.route.as_ref(),
            )
            && crate::vdisplay::dedicated_game_exited(self.live.cur_node_id);
        if !exited {
            return false;
        }
        tracing::info!("dedicated game session: the game exited — ending the session cleanly");
        if let Some(g) = self.game_life.as_ref() {
            crate::gamelease::report_exit(&g.shared());
        }
        self.facts
            .life
            .end(crate::events::SessionEndReason::GameExited);
        self.facts
            .conn
            .close(punktfunk_core::quic::APP_EXITED_CLOSE_CODE, b"game exited");
        true
    }

    /// Only a Linux dedicated game session ends with its game.
    #[cfg(not(target_os = "linux"))]
    fn end_on_game_exit(&mut self) -> bool {
        let _ = (&self.live.cur_node_id, &self.game_life);
        false
    }

    /// [`follow_source_mode`]: reopen the encoder at the delivered size and make it the
    /// session's mode, PyroWave's rate re-resolved for it as on a mode switch, and tell the
    /// client. `Ok(false)` = the reopen failed and the tick is spent on the backoff.
    pub(super) fn follow_source_mode(&mut self) -> Result<bool> {
        let from = self.live.enc_src;
        let to = (
            self.live.frame.format,
            self.live.frame.width,
            self.live.frame.height,
        );
        let reopen = |st: &mut Self| match st.rebuild(RebuildAsk::SourceMode(st.delivered_mode())) {
            Ok(Rebuilt::Kept(e)) => Err(e.context(format!(
                "the capture source changed to {}x{} {:?} mid-session and the encoder could not \
                 be reopened at it",
                to.1, to.2, to.0
            ))),
            other => other.map(drop),
        };
        fn retry(st: &mut StreamState) -> (&mut EncoderWatchdog, &mut std::time::Instant) {
            (&mut st.clock.watchdog, &mut st.next)
        }
        let interval = self.live.interval;
        match follow_source_mode(self, from, to, interval, reopen, retry)? {
            SourceFollow::Unchanged => Ok(true),
            SourceFollow::BackedOff => Ok(false),
            SourceFollow::Reopened(()) => {
                tracing::info!(
                    from = %format!("{}x{} {:?}", from.1, from.2, from.0),
                    to = %format!("{}x{} {:?}", to.1, to.2, to.0),
                    "the capture source changed mode mid-session with no client reconfigure — \
                     reopened the encoder at the delivered size"
                );
                Ok(true)
            }
        }
    }
}

/// What the capture-loss build re-detects each attempt.
impl StreamState {
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
            ..self.display.vd_params.clone()
        }
    }

    /// One capture-loss attempt's re-detection: follow the live session's compositor, opening a
    /// new backend with the session's request when it changed, and re-derive the cursor plan.
    /// Input follows on commit, onto the compositor the build landed on.
    fn retarget_to_live_session(&mut self) {
        let active = crate::vdisplay::detect_active_session();
        crate::vdisplay::observe_session_instance(&active);
        let Some(c) = crate::vdisplay::compositor_for_kind(active.kind) else {
            return;
        };
        crate::vdisplay::apply_session_env(&active);
        let rebuilt_route = crate::vdisplay::resolve_gamescope_route(c, false);
        let mut reopened = false;
        if c != self.display.compositor {
            if matches!(
                c,
                crate::vdisplay::Compositor::Kwin | crate::vdisplay::Compositor::Mutter
            ) {
                crate::vdisplay::settle_desktop_portal(c);
            }
            match crate::vdisplay::open_for(c, self.display.vd_params.client_fp) {
                Ok(v) => {
                    tracing::info!(
                        from = self.display.compositor.id(),
                        to = c.id(),
                        "capture loss: active session switched compositor — retargeting"
                    );
                    self.display.vd = v;
                    self.display.compositor = c;
                    reopened = true;
                }
                Err(e2) => tracing::warn!(error = %format!("{e2:#}"),
                    "capture loss: opening the newly-detected compositor failed — retrying"),
            }
        }
        // Also when only the gamescope route changed: Attach and Spawn differ in who draws.
        let hw_cursor = self.retarget_cursor_plan(self.display.compositor, rebuilt_route.as_ref());
        self.display.vd_params.route = rebuilt_route.clone();
        self.display.vd_params.hw_cursor = hw_cursor;
        if reopened {
            self.reopen_params(hw_cursor, rebuilt_route)
                .apply(&mut *self.display.vd);
        } else {
            // The live display keeps the rest of its request, launch included.
            self.display.vd.set_hw_cursor(hw_cursor);
            self.display.vd.set_gamescope_route(rebuilt_route);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Plane;

    use super::super::swap::tests::{frame, Fake as Session, WIDE};

    /// What a site leaves when nothing new runs: its build failed (the fake's display never
    /// comes up, its encoder never opens) or it had nothing to build. `gap` is `None` where the
    /// failure is too quick to announce.
    struct KeptRow {
        site: &'static str,
        /// Hands the loop its ask and runs the site.
        run: fn(&mut Session),
        gap: Option<bool>,
        /// The client hears the mode that still runs, as a correction.
        corrects: bool,
        codec: Option<crate::native::wiring::FormatOutcome>,
        rate: Option<AckReason>,
        /// The frame open on the wire before the rebuild still is.
        frame_open: bool,
    }

    fn wide() -> punktfunk_core::Mode {
        punktfunk_core::Mode {
            width: WIDE.0,
            height: WIDE.1,
            refresh_hz: 60,
        }
    }

    const KEPT: &[KeptRow] = &[
        KeptRow {
            site: "mode switch to the live mode: nothing rebuilt, the delivered mode told (S5-6)",
            run: |f| {
                let (tx, rx) = std::sync::mpsc::channel();
                f.st.ends.reconfig = rx;
                tx.send((f.st.live.cur_mode, None))
                    .expect("the loop's end is open");
                f.st.on_mode_switch();
            },
            gap: Some(false),
            corrects: true,
            codec: None,
            rate: None,
            frame_open: true,
        },
        KeptRow {
            site: "mode switch (44692a0dc)",
            run: |f| {
                let (tx, rx) = std::sync::mpsc::channel();
                f.st.ends.reconfig = rx;
                tx.send((wide(), None)).expect("the loop's end is open");
                f.st.on_mode_switch();
            },
            gap: Some(true),
            corrects: true,
            codec: None,
            rate: None,
            frame_open: true,
        },
        KeptRow {
            site: "codec change (S5-4)",
            run: |f| {
                let (tx, rx) = std::sync::mpsc::channel();
                f.st.ends.format_rx = rx;
                tx.send(crate::encode::Codec::H264)
                    .expect("the loop's end is open");
                f.st.on_format_change();
            },
            gap: Some(true),
            corrects: false,
            codec: Some(None),
            rate: None,
            frame_open: true,
        },
        KeptRow {
            site: "source mode",
            run: |f| {
                f.st.live.frame = frame(WIDE);
                let followed = f.st.follow_source_mode().expect("within the reset budget");
                assert!(!followed, "the tick goes on the backoff");
            },
            gap: None,
            corrects: false,
            codec: None,
            rate: None,
            frame_open: true,
        },
        KeptRow {
            site: "bitrate (S7-4)",
            run: |f| {
                let (tx, rx) = std::sync::mpsc::channel();
                f.st.ends.bitrate_rx = rx;
                tx.send(f.st.live.bitrate_kbps / 2)
                    .expect("the loop's end is open");
                f.st.on_bitrate_request();
            },
            gap: None,
            corrects: false,
            codec: None,
            rate: Some(AckReason::EncoderLimit),
            frame_open: true,
        },
    ];

    /// A rebuild that lands nothing leaves the session on what it ran: its plan, depth, cursor
    /// plan, display request, rate, mode and epoch. Each column a row names is what the site
    /// tells or keeps.
    #[test]
    fn a_failed_rebuild_keeps_what_runs() {
        for row in KEPT {
            let site = row.site;
            let mut f = super::super::swap::tests::fake();
            f.st.display.vd_params.hdr = true;
            f.st.display.vd_params.hw_cursor = true;
            f.st.clock.wire_frame_open = true;
            let plan = format!("{:?}", f.st.plan);
            let (mode, rate, epoch) = (f.st.live.cur_mode, f.st.live.bitrate_kbps, f.st.live.epoch);
            let delivered = f.st.delivered_mode();

            (row.run)(&mut f);

            assert_eq!(format!("{:?}", f.st.plan), plan, "{site}: plan");
            assert_eq!(f.st.bit_depth, 8, "{site}: depth");
            let d = &f.st.display;
            assert_eq!(
                (d.gamescope_composite, d.metadata_composite),
                (false, false),
                "{site}: composite"
            );
            assert_eq!(
                (d.vd_params.hdr, d.vd_params.hw_cursor),
                (true, true),
                "{site}: display request"
            );
            let shown = f.display.lock().unwrap();
            assert_eq!(
                (shown.hdr.unwrap_or(true), shown.hw_cursor.unwrap_or(true)),
                (true, true),
                "{site}: the display shows the request that runs"
            );
            drop(shown);
            assert_eq!(
                (f.st.live.cur_mode, f.st.live.bitrate_kbps, f.st.live.epoch),
                (mode, rate, epoch),
                "{site}: mode, rate, epoch"
            );
            if let Some(gap) = row.gap {
                assert_eq!(f.gaps.try_recv().is_ok(), gap, "{site}: gap");
            }
            let told = f.told.try_recv().ok().map(|d| (d.mode, d.corrects));
            let corrected = row.corrects.then_some((delivered, true));
            assert_eq!(told, corrected, "{site}: what the client hears");
            assert_eq!(f.codecs.try_recv().ok(), row.codec, "{site}: codec outcome");
            let rate_told = f.rates.try_recv().ok().map(|(_, why)| why);
            assert_eq!(rate_told, row.rate, "{site}: rate outcome");
            assert_eq!(
                f.st.clock.wire_frame_open, row.frame_open,
                "{site}: open wire frame"
            );
        }
    }
    use super::super::swap::tests::{CountsKeyframes, SMALL};
    use crate::native::wiring::FormatOutcome;
    use std::sync::atomic::AtomicU32;

    /// What a scripted build hands the performer.
    #[derive(Clone, Copy)]
    enum Script {
        /// A pipeline of this size, opened at this rate.
        Pipeline((u32, u32), u32),
        /// An encoder alone.
        Encoder,
        Fails,
    }

    const fn mode((width, height): (u32, u32), refresh_hz: u32) -> punktfunk_core::Mode {
        punktfunk_core::Mode {
            width,
            height,
            refresh_hz,
        }
    }

    /// The fake's mode, and what its 16 ms interval delivers at each size.
    const ASKED: punktfunk_core::Mode = mode(SMALL, 60);
    const DELIVERED: punktfunk_core::Mode = mode(SMALL, 63);
    const DELIVERED_WIDE: punktfunk_core::Mode = mode(WIDE, 63);

    /// `rate_kbps` is the plan's, which an encoder-only build opens at.
    fn script(s: Script, rate_kbps: u32, keyframes: &Arc<AtomicU32>) -> Result<Built> {
        // A rebuild stalls the stream: long enough to announce.
        std::thread::sleep(std::time::Duration::from_millis(3));
        let enc = || Box::new(CountsKeyframes(keyframes.clone()));
        match s {
            Script::Pipeline(size, kbps) => Ok(Built::pipeline(
                Pipeline {
                    capturer: Box::new(crate::capture::SyntheticCapturer::new(size.0, size.1, 60)),
                    enc: enc(),
                    frame: frame(size),
                    interval: std::time::Duration::from_millis(16),
                    node_id: 2,
                    display_gen: Some(2),
                    bitrate_kbps: kbps,
                    reframe: Default::default(),
                    #[cfg(target_os = "linux")]
                    lease: None,
                    #[cfg(target_os = "linux")]
                    panel_tick_ns: None,
                },
                None,
            )),
            Script::Encoder => Ok(Built {
                next: Next::Encoder {
                    enc: enc(),
                    reframe: Default::default(),
                    resized: None,
                },
                rate_kbps,
                vd: None,
            }),
            Script::Fails => anyhow::bail!("the scripted build failed"),
        }
    }

    /// One ask performed over a scripted build, and what the session holds after.
    struct PerformRow {
        name: &'static str,
        ask: fn() -> RebuildAsk,
        setup: fn(&mut Session),
        script: Script,
        /// `Some(true)` rebuilt, `Some(false)` kept, `None` the session ends.
        built: Option<bool>,
        compositor: Compositor,
        mode: punktfunk_core::Mode,
        rate_kbps: u32,
        /// Every `Delivered` the control task heard, the swap's own first.
        told: &'static [(punktfunk_core::Mode, bool)],
        codec: Option<FormatOutcome>,
        rate_told: Option<(u32, AckReason)>,
        keyframes: u32,
        gap: bool,
        /// The topology generation followed; `None` = the one there is now.
        seen_reassert_gen: Option<u64>,
        frame_open: bool,
    }

    fn switch() -> RebuildAsk {
        RebuildAsk::SessionSwitch(SessionSwitch {
            kind: crate::vdisplay::ActiveKind::Gaming,
            compositor: Compositor::Gamescope,
            env: None,
            launch: Some("steam".into()),
        })
    }

    fn idd_push(f: &mut Session) {
        f.st.plan.capture = crate::session_plan::CaptureBackend::IddPush;
    }

    const PERFORMED: &[PerformRow] = &[
        PerformRow {
            name: "session switch: the new compositor, its rate, a gap (S5-2)",
            ask: switch,
            setup: |_| {},
            script: Script::Pipeline(WIDE, 30_000),
            built: Some(true),
            compositor: Compositor::Gamescope,
            mode: ASKED,
            rate_kbps: 30_000,
            told: &[(DELIVERED_WIDE, false)],
            codec: None,
            rate_told: Some((30_000, AckReason::Granted)),
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: false,
        },
        PerformRow {
            name: "mode switch: the mode adopted, the delivered one told",
            ask: || RebuildAsk::Mode(mode(WIDE, 60), None),
            setup: |_| {},
            script: Script::Pipeline(WIDE, 20_000),
            built: Some(true),
            compositor: Compositor::Kwin,
            mode: mode(WIDE, 60),
            rate_kbps: 20_000,
            told: &[(DELIVERED_WIDE, false), (DELIVERED_WIDE, true)],
            codec: None,
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: false,
        },
        PerformRow {
            name: "codec change: the control task hears the codec runs",
            ask: || RebuildAsk::Format(crate::encode::Codec::H264),
            setup: |_| {},
            script: Script::Pipeline(SMALL, 20_000),
            built: Some(true),
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 20_000,
            told: &[(DELIVERED, false)],
            codec: Some(Some(crate::encode::Codec::H264)),
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: false,
        },
        PerformRow {
            name: "topology re-assert: the generation read again on commit (S5-9)",
            ask: || RebuildAsk::TopologyReassert(5),
            setup: idd_push,
            script: Script::Pipeline(SMALL, 20_000),
            built: Some(true),
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 20_000,
            told: &[(DELIVERED, false)],
            codec: None,
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: None,
            frame_open: false,
        },
        PerformRow {
            name: "capture lost: a keyframe asked of the new encoder, a gap (S5-2)",
            ask: || RebuildAsk::CaptureLost(None),
            setup: |_| {},
            script: Script::Pipeline(SMALL, 20_000),
            built: Some(true),
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 20_000,
            told: &[(DELIVERED, false)],
            codec: None,
            rate_told: None,
            keyframes: 1,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: false,
        },
        PerformRow {
            name: "source mode: the delivered mode adopted and told as a correction (S5-2)",
            ask: || RebuildAsk::SourceMode(DELIVERED),
            setup: |_| {},
            script: Script::Encoder,
            built: Some(true),
            compositor: Compositor::Kwin,
            mode: DELIVERED,
            rate_kbps: 20_000,
            told: &[(DELIVERED, false), (DELIVERED, true)],
            codec: None,
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: false,
        },
        PerformRow {
            name: "a view alone: the encoder reopens, the client hears the mode it holds",
            ask: || RebuildAsk::Mode(ASKED, Some((8, 8))),
            setup: |_| {},
            script: Script::Encoder,
            built: Some(true),
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 20_000,
            told: &[(DELIVERED, false), (DELIVERED, true)],
            codec: None,
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: false,
        },
        PerformRow {
            name: "failed view change: the view kept, the mode told as a correction",
            ask: || RebuildAsk::Mode(ASKED, Some((8, 8))),
            setup: |_| {},
            script: Script::Fails,
            built: Some(false),
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 20_000,
            told: &[(DELIVERED, true)],
            codec: None,
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: true,
        },
        PerformRow {
            name: "bitrate: the rate runs, the epoch stays, the frame closes on commit",
            ask: || RebuildAsk::Bitrate(10_000),
            setup: |_| {},
            script: Script::Encoder,
            built: Some(true),
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 10_000,
            told: &[],
            codec: None,
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: false,
        },
        PerformRow {
            name: "failed session switch: the backend kept, the stall announced",
            ask: switch,
            setup: |_| {},
            script: Script::Fails,
            built: Some(false),
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 20_000,
            told: &[],
            codec: None,
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: true,
        },
        PerformRow {
            name: "failed mode switch: the mode kept is told as a correction (44692a0dc)",
            ask: || RebuildAsk::Mode(mode(WIDE, 60), None),
            setup: |_| {},
            script: Script::Fails,
            built: Some(false),
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 20_000,
            told: &[(DELIVERED, true)],
            codec: None,
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: true,
        },
        PerformRow {
            name: "failed codec change: the control task hears it failed (S5-4)",
            ask: || RebuildAsk::Format(crate::encode::Codec::H264),
            setup: |_| {},
            script: Script::Fails,
            built: Some(false),
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 20_000,
            told: &[],
            codec: Some(None),
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: true,
        },
        PerformRow {
            name: "failed topology re-assert: the session ends",
            ask: || RebuildAsk::TopologyReassert(5),
            setup: idd_push,
            script: Script::Fails,
            built: None,
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 20_000,
            told: &[],
            codec: None,
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: true,
        },
        PerformRow {
            name: "failed capture loss: the session ends",
            ask: || RebuildAsk::CaptureLost(None),
            setup: |_| {},
            script: Script::Fails,
            built: None,
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 20_000,
            told: &[],
            codec: None,
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: true,
        },
        PerformRow {
            name: "failed source mode: the tick goes on the backoff",
            ask: || RebuildAsk::SourceMode(DELIVERED),
            setup: |_| {},
            script: Script::Fails,
            built: Some(false),
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 20_000,
            told: &[],
            codec: None,
            rate_told: None,
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: true,
        },
        PerformRow {
            name: "failed rate drop: the client hears the rate kept (S7-4)",
            ask: || RebuildAsk::Bitrate(10_000),
            setup: |_| {},
            script: Script::Fails,
            built: Some(false),
            compositor: Compositor::Kwin,
            mode: ASKED,
            rate_kbps: 20_000,
            told: &[],
            codec: None,
            rate_told: Some((20_000, AckReason::EncoderLimit)),
            keyframes: 0,
            gap: true,
            seen_reassert_gen: Some(9),
            frame_open: true,
        },
    ];

    /// The performer over a scripted build: each reason's commit and rollback as its plan says.
    /// A failed build leaves the plan, depth, cursor plan and display request as they were.
    #[test]
    fn each_rebuild_commits_or_rolls_back_as_planned() {
        for row in PERFORMED {
            let name = row.name;
            let mut f = super::super::swap::tests::fake();
            f.st.clock.wire_frame_open = true;
            f.st.seen_reassert_gen = 9;
            f.st.display.vd_params.launch = Some("game".into());
            (row.setup)(&mut f);
            let (plan_before, keyframes) =
                (format!("{:?}", f.st.plan), Arc::new(AtomicU32::new(0)));
            let ask = (row.ask)();
            let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
            let switch = matches!(ask, RebuildAsk::SessionSwitch(_));
            let mut launch = None;
            let outcome = f.st.perform(plan, ask, |_, plan, prep, _| {
                launch = prep.launch.take();
                script(row.script, plan.rate_kbps, &keyframes)
            });

            let built = outcome.ok().map(|r| matches!(r, Rebuilt::Built));
            assert_eq!(built, row.built, "{name}: outcome");
            let st = &f.st;
            assert_eq!(st.display.compositor, row.compositor, "{name}: compositor");
            assert_eq!(st.live.cur_mode, row.mode, "{name}: mode");
            assert_eq!(st.live.bitrate_kbps, row.rate_kbps, "{name}: rate");
            let told: Vec<_> = std::iter::from_fn(|| f.told.try_recv().ok())
                .map(|d| (d.mode, d.corrects))
                .collect();
            assert_eq!(told, row.told, "{name}: what the control task heard");
            assert_eq!(f.codecs.try_recv().ok(), row.codec, "{name}: codec outcome");
            assert_eq!(
                f.rates.try_recv().ok(),
                row.rate_told,
                "{name}: rate outcome"
            );
            let asked = keyframes.load(Ordering::Relaxed);
            assert_eq!(asked, row.keyframes, "{name}: keyframes");
            assert_eq!(f.gaps.try_recv().is_ok(), row.gap, "{name}: gap");
            let seen = row
                .seen_reassert_gen
                .unwrap_or_else(crate::windows::idd::topology_reassert_gen);
            assert_eq!(st.seen_reassert_gen, seen, "{name}: topology generation");
            let open = st.clock.wire_frame_open;
            assert_eq!(open, row.frame_open, "{name}: open wire frame");
            assert_eq!(
                st.display.vd_params.launch.as_deref(),
                Some("game"),
                "{name}: the session's launch stays its own"
            );
            if row.built == Some(false) {
                assert_eq!(format!("{:?}", st.plan), plan_before, "{name}: plan kept");
            }
            let display_launch = switch.then_some("steam");
            assert_eq!(
                launch.as_deref(),
                display_launch,
                "{name}: the new display's launch"
            );
        }
    }

    /// A mode ask's view is in the plan its build opens on; a codec ask keeps it; a later ask
    /// without one clears it.
    #[test]
    fn the_view_rides_the_mode_ask_into_the_plan() {
        use punktfunk_core::video_fit::VideoFit;
        let mut f = super::super::swap::tests::fake();
        let keyframes = Arc::new(AtomicU32::new(0));
        let mut opened_for = Vec::new();
        for ask in [
            RebuildAsk::Mode(ASKED, Some((8, 8))),
            RebuildAsk::Format(crate::encode::Codec::H264),
            RebuildAsk::Mode(ASKED, None),
        ] {
            let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
            let built = f.st.perform(plan, ask, |st, plan, _, _| {
                opened_for.push(st.plan.reframe_to);
                script(Script::Encoder, plan.rate_kbps, &keyframes)
            });
            assert!(matches!(built, Ok(Rebuilt::Built)));
        }
        let view = Some((VideoFit::Fit, (8, 8)));
        assert_eq!(opened_for, [view, view, None]);
        assert_eq!(f.st.plan.codec, crate::encode::Codec::H264);
    }

    /// A new view whose encoder cannot frame it runs PyroWave at the size it opened at, and
    /// opens at that rate.
    #[test]
    fn an_encoder_that_cannot_frame_the_view_reprices_pyrowave() {
        use crate::native::bitrate::pyrowave_mode_kbps;
        let mut f = super::super::swap::tests::fake();
        let full = mode((1920, 1080), 60);
        f.st.live.frame = frame((1920, 1080));
        f.st.live.cur_mode = full;
        f.st.plan.codec = crate::encode::Codec::PyroWave;
        f.st.plan.pyrowave_bpp = 4.0;
        let ask = RebuildAsk::Mode(full, Some((1280, 720)));
        let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("an encoder rebuild");
        let at_mode = pyrowave_mode_kbps(None, &full, &f.st.plan, None);
        assert!(plan.rate_kbps < at_mode, "the plan priced the view");
        let enc_bps = u64::from(f.st.enc_now().enc_kbps(at_mode)) * 1000;
        let mut opened = Vec::new();
        let built = f.st.perform(plan, ask, |st, plan, _, _| {
            st.build_encoder(plan, |_, p| {
                opened.push(((p.width, p.height), p.bitrate_bps));
                Ok(Box::new(super::super::swap::tests::NullEnc))
            })
        });
        assert!(matches!(built, Ok(Rebuilt::Built)));
        assert_eq!(f.st.live.bitrate_kbps, at_mode);
        assert_eq!(opened.last(), Some(&((1920, 1080), enc_bps)));
    }

    use crate::vdisplay::Compositor;
    use std::collections::VecDeque;

    /// A lost capturer whose feed holds `keepalive` until asked for it.
    fn lost_source(keepalive: Option<Box<dyn Send>>) -> pf_capture::FakeCapturer {
        pf_capture::FakeCapturer {
            feed: Some(pf_capture::FakeFeed {
                keepalive,
                ..Default::default()
            }),
            ..Default::default()
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
        fn build(&mut self, first: bool) -> Result<&'static str> {
            self.calls.push(if first { "build first" } else { "build" });
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
                let kept = count_capture_loss(l.plane, &mut rebuilds, lost, &mut lost_source(None));
                assert!(kept.expect("within the count").is_none());
            }
            let lost = anyhow::anyhow!("stream error");
            let e = count_capture_loss(l.plane, &mut rebuilds, lost, &mut lost_source(None))
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
            let mut cap = lost_source(Some(Box::new(())));
            let kept = count_capture_loss(l.plane, &mut rebuilds, alive, &mut cap).unwrap();
            assert!(kept.is_some());
            let mut cap = lost_source(Some(Box::new(())));
            let gone = anyhow::anyhow!("compositor went away");
            let kept = count_capture_loss(l.plane, &mut rebuilds, gone, &mut cap).unwrap();
            assert!(kept.is_none());
            assert!(
                cap.feed.is_some_and(|f| f.keepalive.is_some()),
                "a dead display keeps its keepalive"
            );
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
            assert_eq!(l.calls, ["reattach", "build first", "build"]);
        }
        for mut l in loops() {
            l.builds = [Ok("built")].into();
            rebuild_after_capture_loss(&mut l, None, &budget).unwrap();
            assert_eq!(l.calls, ["build first"], "no keepalive, no re-attach");
        }
    }

    /// A loop's watchdog, next tick and how often it reopened.
    struct Ticks {
        watchdog: EncoderWatchdog,
        next: std::time::Instant,
        reopens: u32,
    }

    /// Native follows at the session's interval, GameStream at the client's frame interval;
    /// both reopen only on a moved source and back off on a failed reopen until the reset
    /// budget is spent.
    #[test]
    fn both_loops_follow_a_moved_source() {
        use pf_frame::PixelFormat;
        let small = (PixelFormat::Bgra, 1280, 720);
        let large = (PixelFormat::Bgra, 1920, 1080);
        let mut ticks = Ticks {
            watchdog: EncoderWatchdog::new(),
            next: std::time::Instant::now(),
            reopens: 0,
        };
        fn retry(t: &mut Ticks) -> (&mut EncoderWatchdog, &mut std::time::Instant) {
            (&mut t.watchdog, &mut t.next)
        }
        for interval in [1.0 / 120.0, 1.0 / 60.0].map(std::time::Duration::from_secs_f64) {
            let ok = |t: &mut Ticks| {
                t.reopens += 1;
                Ok(())
            };
            let same = follow_source_mode(&mut ticks, small, small, interval, ok, retry).unwrap();
            assert!(matches!(same, SourceFollow::Unchanged));
            assert_eq!(ticks.reopens, 0);
            let moved = follow_source_mode(&mut ticks, small, large, interval, ok, retry).unwrap();
            assert!(matches!(moved, SourceFollow::Reopened(())));
            assert_eq!(ticks.reopens, 1);
            ticks.reopens = 0;

            let before = std::time::Instant::now();
            let fail = |_: &mut Ticks| -> Result<()> { anyhow::bail!("driver settling") };
            let held = follow_source_mode(&mut ticks, small, large, interval, fail, retry).unwrap();
            assert!(matches!(held, SourceFollow::BackedOff));
            assert!(ticks.next >= before + std::time::Duration::from_millis(100));
            for _ in 1..MAX_ENCODER_RESETS {
                ticks.watchdog.spend(interval);
            }
            let e = follow_source_mode(&mut ticks, small, large, interval, fail, retry)
                .err()
                .expect("the reset budget is spent");
            assert!(format!("{e:#}").contains("source's new mode"));
            ticks.watchdog.on_au();
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
            assert_eq!(l.calls, ["build first"]);
        }
    }
}
