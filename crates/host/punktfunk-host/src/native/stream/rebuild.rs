//! Everything that swaps the pipeline under a running stream: a Gaming↔Desktop session switch,
//! a client mode switch, a codec ask, content that turned HDR or SDR, the Windows topology
//! re-assert, capture loss, a source that changed size with no client Reconfigure, and a rate
//! the encoder cannot take in place.
//! Each site drains its ask into [`StreamState::rebuild`], the one performer: it plans the
//! rebuild ([`rebuild_plan::plan`]), prepares, builds, then commits through
//! [`StreamState::swap_pipeline`] and the plan's epilogue, or rolls back.

#[cfg(target_os = "linux")]
use super::cursor::settle_portal_cursor;
#[cfg(target_os = "linux")]
use super::pipeline::reattach_pipeline;
use super::pipeline::{
    build_pipeline, build_pipeline_with_retry, is_permanent_build_error, open_session_encoder_with,
    BuildRequest, Pipeline,
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
use crate::native::geometry::{Geometry, Hold, Tenure};

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
        let req = self.build_request(self.live.geometry.intent(), self.live.bitrate_kbps);
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
        let req = self.build_request(self.live.geometry.intent(), self.live.bitrate_kbps);
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
    format_facts: super::super::handshake::FormatFacts,
    composite: (bool, bool),
    params: crate::vdisplay::SessionParams,
    compositor: crate::vdisplay::Compositor,
    tenure: Tenure,
}

/// A rebuild between prepare and commit.
struct Prepared {
    before: Before,
    /// The request the session's displays open with once this commits.
    params: Option<crate::vdisplay::SessionParams>,
    /// The live display was handed the new request's HDR and cursor.
    shown: bool,
    /// The display change was refused: nothing is built and the plan rolls back.
    refused: Option<anyhow::Error>,
    /// The session switch's launch, for the display it opens.
    launch: Option<String>,
    /// The lost capturer's keepalive, for the re-attach.
    keepalive: Option<Box<dyn Send>>,
}

/// The one rebuild performer: [`StreamState::rebuild`] and the steps it runs.
impl StreamState {
    /// What [`rebuild_plan::plan`] reads, the display's holders as the registry has them now.
    fn rebuild_facts(&self) -> RebuildFacts {
        let (tenure, generation) = (self.display.tenure, self.live.cur_display_gen);
        RebuildFacts {
            intent: self.live.geometry.intent(),
            rate_kbps: self.live.bitrate_kbps,
            plan: self.plan,
            compositor: self.display.compositor,
            bitrate_auto: self.facts.terms.bitrate_auto(),
            pyrowave_max_kbps: self.facts.knobs.pyrowave_max_kbps,
            seen_reassert_gen: self.seen_reassert_gen,
            follows_session: pf_host_config::config().compositor.is_none(),
            windows: cfg!(target_os = "windows"),
            tenure,
            hold: tenure.hold(generation, crate::vdisplay::registry::shared),
        }
    }

    /// Plan `ask` and perform it: prepare (snapshot what it changes, apply the display change),
    /// build, then commit or roll back. `Err` only where the plan ends the session on a failed
    /// build. The live mode asked again still tells the control task what is delivered (S5-6).
    pub(super) fn rebuild(&mut self, ask: RebuildAsk) -> Result<Rebuilt> {
        let Some(plan) = rebuild_plan::plan(&ask, &self.rebuild_facts()) else {
            if let RebuildAsk::Mode(asked) = ask {
                self.tell(self.live.geometry.delivered() != asked.mode());
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
        let built = match prep.refused.take() {
            Some(refused) => Err(refused),
            None => build(self, &plan, &mut prep, trace.as_deref()),
        };
        match built {
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

    /// Snapshot what the plan changes, take the ask's payload — the framing of the intent the
    /// plan builds goes into the session plan — and apply its display change:
    /// a joiner that stops joining, a switch's env, route, format and cursor plan, a codec's
    /// (refused where it would relight a display another session is on), or the format of
    /// content that turned HDR or SDR.
    fn prepare(&mut self, plan: &RebuildPlan, ask: RebuildAsk) -> Prepared {
        let d = &self.display;
        let mut prep = Prepared {
            before: Before {
                plan: self.plan,
                bit_depth: self.bit_depth,
                format_facts: self.facts.format_facts.clone(),
                composite: (d.gamescope_composite, d.metadata_composite),
                params: d.vd_params.clone(),
                compositor: d.compositor,
                tenure: d.tenure,
            },
            params: None,
            shown: false,
            refused: None,
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
            RebuildAsk::Mode(asked) => {
                let (new_mode, view) = (asked.mode(), plan.intent.view());
                tracing::info!(?new_mode, ?view, "rebuilding pipeline for mode switch");
            }
            RebuildAsk::TopologyReassert(_) => tracing::info!(
                "exclusive-topology eviction bounced the virtual display's modes — rebuilding \
                 the capture attachment in place at the current mode"
            ),
            RebuildAsk::CaptureLost(keepalive) => prep.keepalive = keepalive,
            // Taking the keepalive stops the running capture: the next one attaches alone.
            RebuildAsk::Content(_) => {
                let feed = self.live.capturer.feed();
                prep.keepalive = feed.and_then(|f| f.take_keepalive());
            }
            RebuildAsk::Format(_) | RebuildAsk::SourceMode(_) | RebuildAsk::Bitrate(_) => {}
        }

        if plan.stops_joining {
            tracing::info!(
                "the display this session joined has no other holder — building its own"
            );
            self.display.tenure.stop_joining();
            self.display.vd_params.join_live = false;
            self.display.vd.set_join_live(false);
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
                let hdr = self.follow_to(to, route.as_ref(), plan.intent.mode());
                let hw_cursor = self.retarget_cursor_plan(to, route.as_ref());
                prep.params = Some(crate::vdisplay::SessionParams {
                    hdr,
                    hw_cursor,
                    route,
                    ..self.display.vd_params.clone()
                });
            }
            Some(DisplayChange::Format(codec)) => {
                let v = self.resolve_live(codec, plan.intent.mode());
                self.show(v.display_hdr, &mut prep);
                tracing::info!(from = ?prep.before.plan.codec, to = ?codec,
                    bit_depth = v.bit_depth, hdr = v.hdr, chroma = ?v.chroma,
                    "rebuilding the pipeline for a codec change");
            }
            Some(DisplayChange::FormatKept(codec)) => {
                let read = self.joiners_read(self.display.vd_params.hdr);
                let v = self.resolve_live(codec, plan.intent.mode());
                if self.joiners_read(v.display_hdr) == read {
                    self.show(v.display_hdr, &mut prep);
                    tracing::info!(from = ?prep.before.plan.codec, to = ?codec,
                        "rebuilding the pipeline for a codec change on a shared display");
                } else {
                    prep.refused = Some(anyhow::anyhow!(
                        "{codec:?} needs the display lit otherwise, and another session is on it"
                    ));
                }
            }
            Some(DisplayChange::Content(hdr)) => {
                self.facts.format_facts.content_hdr = Some(hdr);
                let v = self.resolve_live(self.plan.codec, plan.intent.mode());
                self.show(self.display.vd_params.hdr, &mut prep);
                tracing::info!(
                    content_hdr = hdr,
                    bit_depth = v.bit_depth,
                    hdr = v.hdr,
                    "the content's HDR state moved — rebuilding the capture in its range"
                );
            }
            None => {}
        }
        prep
    }

    /// Run the handshake's depth, HDR and chroma rule on the session's facts for `codec` at
    /// `mode` (S2-5), and resolve the plan for it on `compositor` and `route`.
    fn resolve_format(
        &mut self,
        codec: crate::encode::Codec,
        mode: punktfunk_core::Mode,
        compositor: crate::vdisplay::Compositor,
        route: Option<&crate::vdisplay::GamescopeRoute>,
    ) -> super::super::handshake::Verdict {
        let v = self
            .facts
            .format_facts
            .resolve(codec, &self.facts.terms.encoder, mode);
        self.plan = super::pipeline::plan_for(
            &self.facts.terms,
            codec,
            &v,
            &self.display.tenure,
            compositor,
            route,
        );
        self.bit_depth = v.bit_depth;
        v
    }

    /// The samples the session's capture hands out as the plan stands.
    fn sample_format(&self) -> crate::vdisplay::admission::SampleFormat {
        crate::vdisplay::admission::SampleFormat {
            bit_depth: self.bit_depth,
            chroma_444: self.plan.chroma.is_444(),
            full_range: self.plan.full_range(self.plan.hdr, self.bit_depth),
            hdr: self.plan.hdr,
        }
    }

    /// What a session joined to this display took at admission and does not follow: whether
    /// the display is lit in `hdr`, and the samples of a capture it shares.
    fn joiners_read(&self, hdr: bool) -> (bool, Option<crate::vdisplay::admission::SampleFormat>) {
        let shares = crate::vdisplay::admission::shares_capture(Some(self.display.compositor));
        (hdr, shares.then(|| self.sample_format()))
    }

    /// [`Self::resolve_format`] on the display the session runs.
    fn resolve_live(
        &mut self,
        codec: crate::encode::Codec,
        mode: punktfunk_core::Mode,
    ) -> super::super::handshake::Verdict {
        let route = self.display.vd_params.route.clone();
        self.resolve_format(codec, mode, self.display.compositor, route.as_ref())
    }

    /// Hand the live display `hdr` and the plan's cursor (S5-4), and make them its request.
    fn show(&mut self, hdr: bool, prep: &mut Prepared) {
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
    }

    /// Whether the display of a session on its way to `to` is lit in HDR. A session that
    /// follows its source runs the format rule for the new one; a session on a display
    /// another is on keeps the format they share.
    fn follow_to(
        &mut self,
        to: crate::vdisplay::Compositor,
        route: Option<&crate::vdisplay::GamescopeRoute>,
        mode: punktfunk_core::Mode,
    ) -> bool {
        if !self.facts.format_facts.follows || self.rebuild_facts().hold != Hold::Alone {
            return self.display.vd_params.hdr;
        }
        self.facts.format_facts = self.facts.format_facts.moved_to(to, route);
        self.resolve_format(self.plan.codec, mode, to, route)
            .display_hdr
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
                let mut vd =
                    crate::vdisplay::open_for(to, client.client_fp, client.screen.clone())?;
                let params = crate::vdisplay::SessionParams {
                    launch: prep.launch.take(),
                    ..prep
                        .params
                        .clone()
                        .unwrap_or_else(|| self.display.vd_params.clone())
                };
                params.apply(&mut *vd);
                let req = self.build_request(plan.intent, plan.rate_kbps);
                let pipe = build_on(&mut vd, &req, &self.facts.life, None, build, trace)?;
                Ok(Built::pipeline(pipe, Some(vd)))
            }
            (build, _) => {
                let req = self.build_request(plan.intent, plan.rate_kbps);
                let live_gen = self.live.cur_display_gen;
                let vd = &mut self.display.vd;
                let pipe = build_on(vd, &req, &self.facts.life, live_gen, build, trace)?;
                Ok(Built::pipeline(pipe, None))
            }
        }
    }

    /// The rate a build for `plan` runs once its encoder took `built`: a rate the plan priced
    /// follows the picture it encodes ([`Geometry::rate_kbps`]), an asked one stands.
    fn pricing<'a>(&self, plan: &'a RebuildPlan) -> impl Fn(&Geometry) -> u32 + use<'a> {
        let (auto, cap) = (
            self.facts.terms.bitrate_auto(),
            self.facts.knobs.pyrowave_max_kbps,
        );
        let session = self.plan;
        move |built| match plan.epilogue.rate {
            Rate::Built => built.rate_kbps(plan.rate_kbps, auto, &session, cap),
            Rate::Applied => plan.rate_kbps,
        }
    }

    /// The encoder alone, on the live capture at the rate it runs, through `open_video`, at the
    /// plan's rate as [`Self::pricing`] holds it. Its geometry is the live one reframed.
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
        let (intent, live, price) = (plan.intent, self.live.geometry, self.pricing(plan));
        let (enc, framing) = open_session_encoder_with(
            &self.facts.terms.encoder,
            &self.plan,
            &*self.live.capturer,
            &self.live.frame,
            intent.framing(),
            live.hz(),
            |f| ed.enc_kbps(price(&live.reframed(intent, *f))) as u64 * 1000,
            self.bit_depth,
            self.facts.terms.client_hdr(),
            wire_seq_base,
            open_video,
        )?;
        let geometry = live.reframed(intent, framing);
        let rate_kbps = price(&geometry);
        let next = Next::Encoder {
            enc,
            geometry,
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
    /// opened at, as [`Self::pricing`] holds it. `None` = the full build runs.
    #[cfg(target_os = "windows")]
    fn in_place(&mut self, plan: &RebuildPlan, trace: &Trace) -> Option<(Next, u32)> {
        let recover_ring = plan.reason == super::swap::SwapReason::TopologyReassert;
        let price = self.pricing(plan);
        let next = self.resize(plan.intent, &price, trace, recover_ring)?;
        let rate_kbps = match &next {
            Next::Encoder { geometry, .. } => price(geometry),
            _ => plan.rate_kbps,
        };
        Some((next, rate_kbps))
    }

    /// Only a Windows IDD-push display is mode-set in place.
    #[cfg(not(target_os = "windows"))]
    fn in_place(&mut self, _plan: &RebuildPlan, _trace: &Trace) -> Option<(Next, u32)> {
        None
    }

    /// Swap the build in, then the plan's epilogue: the display request it commits to, what later
    /// joiners read off it (colour, format, and the compositor and route it moved to), the input
    /// route, the rate, the further screens, who hears the mode, the gap. The swap's tell gives
    /// later joiners the mode.
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
        let format = self.sample_format();
        // The request every later display of this session starts from.
        if let Some(params) = prep.params {
            let stop = self.facts.life.stop_flag();
            crate::vdisplay::admission::publish_hdr(stop, params.hdr);
            crate::vdisplay::admission::publish_format(stop, format);
            self.display.vd_params = params;
        }
        // A capture loss retargets in its build, with no switch in the plan: compare, not ask.
        let moved = self.display.compositor != prep.before.compositor;
        if moved || self.display.vd_params.route != prep.before.params.route {
            let d = &self.display;
            crate::vdisplay::admission::publish_display(
                self.facts.life.stop_flag(),
                d.compositor,
                d.vd_params.route.clone(),
                d.vd_params.hdr,
                format,
            );
        }
        if e.input {
            self.repoint_input();
        }
        match e.rate {
            Rate::Built => self.adopt_built_bitrate(built.rate_kbps),
            Rate::Applied => self.adopt_rebuilt_rate(plan.rate_kbps),
        }
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
        let m = plan.intent.mode();
        if e.keyframe {
            self.live.enc.request_keyframe();
        }
        // The pacing clock restarts: the stall was the rebuild's, not encode falling behind.
        self.next = std::time::Instant::now();
        match (e.tell, plan.display) {
            (Some(Tell::Mode), _) => self.tell(self.live.geometry.delivered() != m),
            (Some(Tell::Correction), _) => self.tell(true),
            (
                Some(Tell::Codec),
                Some(DisplayChange::Format(codec) | DisplayChange::FormatKept(codec)),
            ) => {
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

    /// Put back what prepare changed, a joiner's tenure among it, then announce the stall and
    /// tell what still runs, as the plan's rollback says.
    fn roll_back(&mut self, plan: &RebuildPlan, prep: Prepared, t0: std::time::Instant) {
        let Before {
            plan: old_plan,
            bit_depth,
            format_facts,
            composite,
            params,
            tenure,
            ..
        } = prep.before;
        (self.plan, self.bit_depth) = (old_plan, bit_depth);
        self.facts.format_facts = format_facts;
        let d = &mut self.display;
        (d.gamescope_composite, d.metadata_composite) = composite;
        if prep.shown {
            d.vd.set_hdr(params.hdr);
            d.vd.set_hw_cursor(params.hw_cursor);
        }
        if plan.stops_joining {
            d.vd.set_join_live(params.join_live);
        }
        (d.vd_params, d.tenure) = (params, tenure);
        if plan.rollback.gap {
            announce_gap_since(&self.ends.gap_tx, t0);
        }
        match plan.rollback.tell {
            Some(Tell::Mode) => self.tell(true),
            Some(Tell::Codec) => {
                let _ = self.ends.format_result_tx.send(None);
            }
            Some(Tell::Rate) => self.keep_rate(plan.rate_kbps),
            Some(Tell::Correction) | None => {}
        }
    }
}

/// Every rebuild site but the rate's ([`StreamState::on_bitrate_request`]): each drains its ask
/// and hands it to [`StreamState::rebuild`].
impl StreamState {
    /// Follow the latest session switch, the watcher's or the seat's: rebuild the backend in
    /// place, with the session's display request, the switch's launch and the cursor plan of the
    /// compositor it switches to, and keep streaming. A launched game the old session takes
    /// down with it does not end the stream. A session on its own gamescope spawn is not the
    /// box's, and stays where it is when the box switches.
    pub(super) fn on_session_switch(&mut self) {
        let Some(sw) = self.ends.session_rx.try_iter().last() else {
            return;
        };
        #[cfg(target_os = "linux")]
        if sw.env.is_some()
            && crate::vdisplay::launch_is_nested(
                self.display.compositor,
                self.display.vd_params.route.as_ref(),
            )
        {
            tracing::debug!(
                to = ?sw.kind,
                "session switch: the box switched under a session on its own gamescope — staying"
            );
            return;
        }
        // The box's sessions end on a switch, and so does a seat's Game Mode; a seat's desktop
        // runs on under its Game Mode. A switch to the compositor the stream is on moves
        // nothing: the capture-loss rebuild followed it first.
        let leaves_its_apps = sw.compositor != self.display.compositor
            && (sw.env.is_some()
                || self.display.compositor == crate::vdisplay::Compositor::Gamescope);
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
        let Some(asked) = self.ends.reconfig.try_iter().last() else {
            return;
        };
        let new_mode = asked.mode();
        if let Ok(Rebuilt::Kept(e)) | Err(e) = self.rebuild(RebuildAsk::Mode(asked)) {
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

    /// The content on the session's gamescope turned HDR or SDR: stop the capture and attach
    /// it again in that range, the display lit as it is. `Err` when no capture comes back up.
    /// A display that is not lit in HDR streams SDR whatever it shows, and is left alone. A
    /// screen with company streams as it is lit, and a capture a joiner is on stays as it is:
    /// the watcher's next look asks again.
    #[cfg(target_os = "linux")]
    pub(super) fn on_content(&mut self) -> Result<()> {
        let streamed = (self.facts.format_facts.content_hdr).filter(|_| self.display.vd_params.hdr);
        let generation = self.live.cur_display_gen;
        let Some(follow) = self.content.as_mut() else {
            return Ok(());
        };
        follow.point(streamed.is_some(), generation);
        let Some((streamed, look)) = streamed.zip(follow.look()) else {
            return Ok(());
        };
        let stop = self.facts.life.stop_flag();
        let hdr = look || crate::vdisplay::admission::company(stop);
        if hdr == streamed {
            return Ok(());
        }
        // Held until the new range is published: a join in between reads neither.
        let Some(_claim) = crate::vdisplay::admission::move_format(stop) else {
            return Ok(());
        };
        self.rebuild(RebuildAsk::Content(hdr))
            .context("re-attach capture in the content's range")?;
        Ok(())
    }

    /// Only gamescope names the HDR state of its content.
    #[cfg(not(target_os = "linux"))]
    pub(super) fn on_content(&mut self) -> Result<()> {
        Ok(())
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
        // What the live geometry delivers on the source's new frame.
        let source = punktfunk_core::video_fit::Reframe::full((to.1, to.2));
        let reopen = |st: &mut Self| {
            let live = st.live.geometry;
            let delivered = live.reframed(live.intent(), source).delivered();
            match st.rebuild(RebuildAsk::SourceMode(delivered)) {
                Ok(Rebuilt::Kept(e)) => Err(e.context(format!(
                    "the capture source changed to {}x{} {:?} mid-session and the encoder could \
                     not be reopened at it",
                    to.1, to.2, to.0
                ))),
                other => other.map(drop),
            }
        };
        fn retry(st: &mut StreamState) -> (&mut EncoderWatchdog, &mut std::time::Instant) {
            (&mut st.clock.watchdog, &mut st.next)
        }
        let interval = self.live.geometry.interval();
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
    /// new backend with the session's request when it changed, at the format the rule gives
    /// that source, and re-derive the cursor plan. Input follows on commit, onto the compositor
    /// the build landed on.
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
            let params = &self.display.vd_params;
            match crate::vdisplay::open_for(c, params.client_fp, params.screen.clone()) {
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
        if reopened {
            self.display.vd_params.hdr = self.follow_to(
                c,
                rebuilt_route.as_ref(),
                self.live.geometry.intent().mode(),
            );
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
pub(crate) mod tests {
    use super::*;
    use crate::events::Plane;

    use super::super::swap::tests::{frame, Fake as Session, WIDE};
    use crate::native::geometry::tests::asked as ask;
    use crate::native::geometry::Intent;
    use punktfunk_core::video_fit::Reframe;

    /// The fake running `intent` on its live frame, one frame every `interval`, framed as
    /// `framing`.
    fn running(f: &mut Session, intent: Intent, interval: std::time::Duration, framing: Reframe) {
        let size = (f.st.live.frame.width, f.st.live.frame.height);
        f.st.live.geometry = Geometry::of(intent, size, interval, framing);
    }

    /// The fake framed for `view` at the mode it runs.
    fn frame_at(f: &mut Session, view: Option<(punktfunk_core::video_fit::VideoFit, (u32, u32))>) {
        let g = f.st.live.geometry;
        running(
            f,
            Intent::of(g.intent().mode(), view),
            g.interval(),
            g.reframe(),
        );
    }

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
            refresh_mhz: 0,
        }
    }

    const KEPT: &[KeptRow] = &[
        KeptRow {
            site: "mode switch to the live mode: nothing rebuilt, the delivered mode told (S5-6)",
            run: |f| {
                let (tx, rx) = std::sync::mpsc::channel();
                f.st.ends.reconfig = rx;
                tx.send(ask(f.st.live.geometry.intent().mode(), None))
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
                tx.send(ask(wide(), None)).expect("the loop's end is open");
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
    #[cfg_attr(
        target_os = "macos",
        ignore = "VideoToolbox opens the 16x16 test size, so no rebuild is refused"
    )]
    fn a_failed_rebuild_keeps_what_runs() {
        for row in KEPT {
            let site = row.site;
            let mut f = super::super::swap::tests::fake();
            f.st.display.vd_params.hdr = true;
            f.st.display.vd_params.hw_cursor = true;
            f.st.clock.wire_frame_open = true;
            let plan = format!("{:?}", f.st.plan);
            let live = &f.st.live;
            let (mode, rate, epoch) = (live.geometry.intent(), live.bitrate_kbps, live.epoch);
            let delivered = live.geometry.delivered();

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
                (
                    f.st.live.geometry.intent(),
                    f.st.live.bitrate_kbps,
                    f.st.live.epoch
                ),
                (mode, rate, epoch),
                "{site}: mode, rate, epoch"
            );
            if let Some(gap) = row.gap {
                assert_eq!(f.gaps.try_recv().is_ok(), gap, "{site}: gap");
            }
            let told = f.told.try_recv().ok().map(|d| (d.delivered, d.corrects));
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
            refresh_mhz: 0,
        }
    }

    /// The fake's mode, and what its 16 ms interval delivers at each size: 62.5 Hz.
    const ASKED: punktfunk_core::Mode = mode(SMALL, 60);
    const INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
    const DELIVERED: punktfunk_core::Mode = punktfunk_core::Mode::exact(SMALL.0, SMALL.1, 62_500);
    const DELIVERED_WIDE: punktfunk_core::Mode =
        punktfunk_core::Mode::exact(WIDE.0, WIDE.1, 62_500);

    /// `plan`'s build, at its intent: an encoder-only one on the fake's capture opens at the
    /// plan's rate.
    fn script(s: Script, plan: &RebuildPlan, keyframes: &Arc<AtomicU32>) -> Result<Built> {
        // A rebuild stalls the stream: long enough to announce.
        std::thread::sleep(std::time::Duration::from_millis(3));
        let enc = || Box::new(CountsKeyframes(keyframes.clone()));
        let at = |size| Geometry::of(plan.intent, size, INTERVAL, Default::default());
        match s {
            Script::Pipeline(size, kbps) => Ok(Built::pipeline(
                Pipeline {
                    capturer: Box::new(crate::capture::SyntheticCapturer::new(size.0, size.1, 60)),
                    enc: enc(),
                    frame: frame(size),
                    geometry: at(size),
                    node_id: 2,
                    display_gen: Some(2),
                    bitrate_kbps: kbps,
                    #[cfg(target_os = "linux")]
                    lease: None,
                },
                None,
            )),
            Script::Encoder => Ok(Built {
                next: Next::Encoder {
                    enc: enc(),
                    geometry: at(SMALL),
                    resized: None,
                },
                rate_kbps: plan.rate_kbps,
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

    /// The display change a seat's switch to `game` plans for a session built on `built`.
    #[cfg(target_os = "linux")]
    pub(in crate::native::stream) fn seat_switch(
        built: Compositor,
        game: bool,
    ) -> Option<DisplayChange> {
        let mut f = super::super::swap::tests::fake();
        f.st.display.compositor = built;
        let ask = RebuildAsk::SessionSwitch(SessionSwitch::seat(game));
        rebuild_plan::plan(&ask, &f.st.rebuild_facts()).and_then(|p| p.display)
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
            ask: || RebuildAsk::Mode(ask(mode(WIDE, 60), None)),
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
            ask: || RebuildAsk::Mode(ask(ASKED, Some((8, 8)))),
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
            ask: || RebuildAsk::Mode(ask(ASKED, Some((8, 8)))),
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
            ask: || RebuildAsk::Mode(ask(mode(WIDE, 60), None)),
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
                script(row.script, plan, &keyframes)
            });

            let built = outcome.ok().map(|r| matches!(r, Rebuilt::Built));
            assert_eq!(built, row.built, "{name}: outcome");
            let st = &f.st;
            assert_eq!(st.display.compositor, row.compositor, "{name}: compositor");
            assert_eq!(st.live.geometry.intent().mode(), row.mode, "{name}: mode");
            assert_eq!(st.live.bitrate_kbps, row.rate_kbps, "{name}: rate");
            let told: Vec<_> = std::iter::from_fn(|| f.told.try_recv().ok())
                .map(|d| (d.delivered, d.corrects))
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
            RebuildAsk::Mode(ask(ASKED, Some((8, 8)))),
            RebuildAsk::Format(crate::encode::Codec::H264),
            RebuildAsk::Mode(ask(ASKED, None)),
        ] {
            let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
            let built = f.st.perform(plan, ask, |_, plan, _, _| {
                opened_for.push(plan.intent.framing());
                script(Script::Encoder, plan, &keyframes)
            });
            assert!(matches!(built, Ok(Rebuilt::Built)));
        }
        let view = Some((VideoFit::Fit, (8, 8)));
        assert_eq!(opened_for, [view, view, None]);
        assert_eq!(f.st.plan.codec, crate::encode::Codec::H264);
    }

    /// Content that turned HDR leaves the display lit as it is. The fact stays once its
    /// capture is built; a build that never comes up puts the old one back and ends the session.
    #[test]
    fn a_content_rebuild_keeps_the_display_lit_and_the_fact_it_built() {
        let mut f = super::super::swap::tests::fake();
        let keyframes = Arc::new(AtomicU32::new(0));
        f.st.facts.format_facts.content_hdr = Some(false);
        f.st.display.vd_params.hdr = true;
        for (build, fact) in [
            (Script::Fails, Some(false)),
            (Script::Pipeline(WIDE, 20_000), Some(true)),
        ] {
            let ask = RebuildAsk::Content(true);
            let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
            let out =
                f.st.perform(plan, ask, |_, plan, _, _| script(build, plan, &keyframes));
            assert_eq!(matches!(out, Ok(Rebuilt::Built)), fact == Some(true));
            assert_eq!(f.st.facts.format_facts.content_hdr, fact);
            assert!(f.st.display.vd_params.hdr, "the display stays lit");
        }
    }

    /// Another session holds the display: a smaller size asked reopens the encoder framed for
    /// it at the live mode, and the client, acked with the size it asked for, hears that mode.
    #[test]
    fn a_shared_display_keeps_its_mode_and_frames_the_asked_size() {
        use punktfunk_core::video_fit::VideoFit;
        let mut f = super::super::swap::tests::fake();
        let keyframes = Arc::new(AtomicU32::new(0));
        let ask = RebuildAsk::Mode(ask(mode((8, 8), 60), None));
        let facts = RebuildFacts {
            hold: Hold::Shared,
            ..f.st.rebuild_facts()
        };
        let plan = rebuild_plan::plan(&ask, &facts).expect("an encoder rebuild");
        assert_eq!(
            (plan.build.clone(), plan.intent.mode()),
            (Build::Encoder, ASKED)
        );
        let mut opened_for = None;
        let built = f.st.perform(plan, ask, |_, plan, _, _| {
            opened_for = plan.intent.framing();
            script(Script::Encoder, plan, &keyframes)
        });
        assert!(matches!(built, Ok(Rebuilt::Built)));
        assert_eq!(opened_for, Some((VideoFit::Fit, (8, 8))));
        assert_eq!(
            f.st.live.geometry.intent().mode(),
            ASKED,
            "the display's mode stays"
        );
        let told: Vec<_> = std::iter::from_fn(|| f.told.try_recv().ok())
            .map(|d| (d.delivered, d.corrects))
            .collect();
        assert_eq!(told, [(DELIVERED, false), (DELIVERED, true)]);
    }

    /// A joiner alone on its display builds as an owner: it stops joining once the build
    /// commits, and joins again, display and control task both, when it fails.
    #[test]
    fn a_lone_joiner_builds_a_display_of_its_own() {
        let keyframes = Arc::new(AtomicU32::new(0));
        for (built, joins_after) in [
            (Script::Pipeline(WIDE, 20_000), false),
            (Script::Fails, true),
        ] {
            let mut f = super::super::swap::tests::fake();
            let joiner = Tenure::of(Some((Default::default(), SMALL)), false);
            f.st.display.tenure = joiner;
            f.st.display.vd_params.join_live = true;
            f.st.shared.tenure.adopt(&joiner, Some(1));
            let ask = RebuildAsk::Mode(ask(mode(WIDE, 60), None));
            let facts = RebuildFacts {
                hold: Hold::Alone,
                ..f.st.rebuild_facts()
            };
            let plan = rebuild_plan::plan(&ask, &facts).expect("a rebuild");
            assert!(plan.stops_joining);
            let mut joined_in_build = None;
            let _ = f.st.perform(plan, ask, |st, plan, _, _| {
                joined_in_build = Some(st.display.vd_params.join_live);
                script(built, plan, &keyframes)
            });
            assert_eq!(
                joined_in_build,
                Some(false),
                "the build acquires as an owner"
            );
            assert_eq!(f.st.display.vd_params.join_live, joins_after);
            assert_eq!(f.st.display.tenure.join_live(), joins_after);
            assert_eq!(f.display.lock().unwrap().join_live, Some(joins_after));
            // Every other holder: only a joiner's display is held.
            let held = f.st.shared.tenure.held(|_| true);
            assert_eq!(held, joins_after, "what the control task reads");
        }
    }

    /// A new view whose encoder cannot frame it runs PyroWave at the size it opened at, and
    /// opens at that rate.
    #[test]
    fn an_encoder_that_cannot_frame_the_view_reprices_pyrowave() {
        use crate::native::bitrate::pyrowave_mode_kbps;
        use crate::native::geometry::Picture;
        let mut f = super::super::swap::tests::fake();
        let full = mode((1920, 1080), 60);
        f.st.live.frame = frame((1920, 1080));
        let g = f.st.live.geometry;
        running(&mut f, Intent::of(full, None), g.interval(), g.reframe());
        f.st.plan.codec = crate::encode::Codec::PyroWave;
        f.st.plan.pyrowave_bpp = 4.0;
        let ask = RebuildAsk::Mode(ask(full, Some((1280, 720))));
        let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("an encoder rebuild");
        let at_mode = pyrowave_mode_kbps(None, &Picture::of(full), &f.st.plan, None);
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

    /// How a scripted `open_video` answers a framed open.
    #[derive(Clone, Copy, Debug)]
    enum Open {
        /// It opens, and the encoder frames on ingest.
        Frames,
        /// It opens, and the encoder neither crops nor scales.
        CannotFrame,
        /// Only an unframed open succeeds.
        FailsFramed,
    }

    /// A scripted `open_video` on a `captured` source that answers as `how`, and the sizes it
    /// was asked to open at.
    fn opener(
        how: Open,
        captured: (u32, u32),
        opened: &std::sync::Mutex<Vec<(u32, u32)>>,
    ) -> impl FnMut(
        &crate::encode::Resolved,
        crate::encode::OpenParams,
    ) -> Result<Box<dyn crate::encode::Encoder>>
           + '_ {
        use crate::session_plan::tests::Frames;
        move |_, p| {
            opened.lock().unwrap().push((p.width, p.height));
            match how {
                Open::FailsFramed if (p.width, p.height) != captured => {
                    anyhow::bail!("no scale pass")
                }
                Open::CannotFrame => Ok(Box::new(Frames(false, Default::default()))),
                _ => Ok(Box::new(Frames(true, Default::default()))),
            }
        }
    }

    /// The fake mid-stream at 1920×1080 and 60 Hz exactly, on its own display.
    fn hd_session() -> Session {
        let mut f = super::super::swap::tests::fake();
        let hd = mode((1920, 1080), 60);
        f.st.live.frame = frame((1920, 1080));
        let framing = f.st.live.geometry.reframe();
        running(&mut f, Intent::of(hd, None), hd.frame_interval(), framing);
        f
    }

    /// PyroWave at 1.6 bits per pixel; `auto` runs Automatic at 150 Mbit/s.
    fn pyrowave(f: &mut Session, auto: bool) {
        f.st.plan.codec = crate::encode::Codec::PyroWave;
        f.st.plan.pyrowave_bpp = 1.6;
        let terms = Arc::get_mut(&mut f.st.facts.terms).expect("the fake holds the only terms");
        terms.codec = crate::encode::Codec::PyroWave;
        if auto {
            (terms.hello.bitrate_kbps, f.st.live.bitrate_kbps) = (0, 150_000);
        }
    }

    /// `ask` performed as `StreamState::rebuild` performs it, with the encoder opened through
    /// `how`; `shared` stands in for another session on the display. `false`: nothing built.
    fn rebuild_with(f: &mut Session, ask: RebuildAsk, shared: bool, how: Open) -> bool {
        let facts = RebuildFacts {
            hold: if shared { Hold::Shared } else { Hold::Alone },
            ..f.st.rebuild_facts()
        };
        let Some(plan) = rebuild_plan::plan(&ask, &facts) else {
            if let RebuildAsk::Mode(asked) = ask {
                f.st.tell(f.st.live.geometry.delivered() != asked.mode());
            }
            return false;
        };
        let captured = (f.st.live.frame.width, f.st.live.frame.height);
        let opened = std::sync::Mutex::new(Vec::new());
        let out = f.st.perform(plan, ask, |st, plan, _, _| {
            st.build_encoder(plan, opener(how, captured, &opened))
        });
        matches!(out, Ok(Rebuilt::Built))
    }

    /// Every `Delivered` the control task heard, as `(mode, encoded, corrects)`.
    fn heard(f: &mut Session) -> Vec<(punktfunk_core::Mode, (u32, u32), bool)> {
        std::iter::from_fn(|| f.told.try_recv().ok())
            .map(|d| {
                let p = d.picture.mode();
                (d.delivered, (p.width, p.height), d.corrects)
            })
            .collect()
    }

    /// One encoder-only build for a 1280×720 view of the live 1920×1080: how its open answers,
    /// what it was asked to open at, and the framing, rate and picture told after.
    struct EncoderRow {
        name: &'static str,
        pyrowave: Option<bool>,
        open: Open,
        opened: Vec<(u32, u32)>,
        encoded: (u32, u32),
        rate_kbps: u32,
        verdict: crate::native::tests::Verdict,
    }

    #[rustfmt::skip]
    fn encoder_rows() -> Vec<EncoderRow> {
        use crate::native::tests::Verdict::Wanted;
        use Open::{CannotFrame, FailsFramed, Frames};
        let (view, hd) = ((1280, 720), (1920, 1080));
        let row = |name, pyrowave, open, opened: &[(u32, u32)], encoded, rate_kbps, verdict| {
            EncoderRow { name, pyrowave, open, opened: opened.to_vec(), encoded, rate_kbps, verdict }
        };
        vec![
            row("HEVC framed: the rate stands", None, Frames, &[view], view, 20_000, Wanted),
            row("HEVC, the framed open fails: the capture, the rate stands", None, FailsFramed,
                &[view, hd], hd, 20_000, Wanted),
            row("HEVC, a backend that cannot frame: the capture", None, CannotFrame, &[view, hd],
                hd, 20_000, Wanted),
            row("PyroWave framed: priced at the view", Some(false), Frames, &[view], view, 88_473,
                Wanted),
            row("PyroWave, the framed open fails: re-priced at the capture", Some(false),
                FailsFramed, &[view, hd], hd, 199_065, Wanted),
            row("PyroWave, a backend that cannot frame: re-priced at the capture", Some(false),
                CannotFrame, &[view, hd], hd, 199_065, Wanted),
            row("PyroWave Automatic, the framed open fails: the running rate inside the \
                 capture's bounds", Some(true), FailsFramed, &[view, hd], hd, 88_473, Wanted),
        ]
    }

    /// The encoder-only build for a new view at the live mode: the size it opens at, the rate it
    /// runs, and the picture the client hears, when the framed open works, fails or cannot frame.
    #[test]
    fn geometry_rows() {
        for r in encoder_rows() {
            let name = format!("{} ({:?})", r.name, r.verdict);
            let mut f = hd_session();
            if let Some(auto) = r.pyrowave {
                pyrowave(&mut f, auto);
            }
            let hd = f.st.live.geometry.intent().mode();
            let ask = RebuildAsk::Mode(ask(hd, Some((1280, 720))));
            let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
            assert_eq!(plan.build, Build::Encoder, "{name}");
            let opened = std::sync::Mutex::new(Vec::new());
            let built = f.st.perform(plan, ask, |st, plan, _, _| {
                st.build_encoder(plan, opener(r.open, (1920, 1080), &opened))
            });
            assert!(matches!(built, Ok(Rebuilt::Built)), "{name}");
            assert_eq!(opened.into_inner().unwrap(), r.opened, "{name}: opens");
            let encoded = f.st.shared.frame_map.lock().unwrap().out;
            assert_eq!(encoded, r.encoded, "{name}: framing");
            assert_eq!(f.st.live.bitrate_kbps, r.rate_kbps, "{name}: rate");
            let told = heard(&mut f).last().map(|&(m, e, _)| (m, e));
            assert_eq!(told, Some((hd, r.encoded)), "{name}: told");
        }
    }

    /// A build's framing: the 16×16 capture scaled to 8×8.
    fn framing() -> punktfunk_core::video_fit::Reframe {
        punktfunk_core::video_fit::Reframe {
            source: SMALL,
            crop: [0, 0, SMALL.0, SMALL.1],
            out: (8, 8),
        }
    }

    /// `built` with the build's framing as [`framing`].
    fn framed(built: Result<Built>) -> Result<Built> {
        let mut built = built?;
        let geometry = match &mut built.next {
            Next::Pipeline(p) => &mut p.geometry,
            Next::Encoder { geometry, .. } => geometry,
        };
        *geometry = geometry.reframed(geometry.intent(), framing());
        Ok(built)
    }

    /// One swap reason and the build that lands it.
    struct MapRow {
        name: &'static str,
        ask: fn() -> RebuildAsk,
        setup: fn(&mut Session),
        script: Script,
        /// The client hears the new epoch.
        told: bool,
        verdict: crate::native::tests::Verdict,
    }

    #[rustfmt::skip]
    fn map_rows() -> Vec<MapRow> {
        use crate::native::tests::Verdict::Wanted;
        let row = |name, ask, setup, script, told, verdict| MapRow {
            name, ask, setup, script, told, verdict,
        };
        let pipe = Script::Pipeline(SMALL, 20_000);
        vec![
            row("session switch", switch as fn() -> RebuildAsk, (|_| {}) as fn(&mut Session),
                pipe, true, Wanted),
            row("mode switch", || RebuildAsk::Mode(ask(mode(WIDE, 60), None)), |_| {}, pipe, true,
                Wanted),
            row("a view alone", || RebuildAsk::Mode(ask(ASKED, Some((8, 8)))), |_| {}, Script::Encoder,
                true, Wanted),
            row("topology re-assert", || RebuildAsk::TopologyReassert(5), idd_push, pipe, true,
                Wanted),
            row("capture lost", || RebuildAsk::CaptureLost(None), |_| {}, pipe, true, Wanted),
            row("source mode", || RebuildAsk::SourceMode(DELIVERED), |_| {}, Script::Encoder, true,
                Wanted),
            row("codec change", || RebuildAsk::Format(crate::encode::Codec::H264), |_| {}, pipe,
                true, Wanted),
            row("content turned HDR", || RebuildAsk::Content(true), |_| {}, pipe, true, Wanted),
            row("rate the encoder could not take in place", || RebuildAsk::Bitrate(10_000),
                |_| {}, Script::Encoder, false, Wanted),
        ]
    }

    /// After each swap reason the input plane's frame map is the framing the build encodes, and
    /// every epoch the swap moved is told at that encoded size.
    #[test]
    fn frame_map_rows() {
        let keyframes = Arc::new(AtomicU32::new(0));
        for r in map_rows() {
            let name = format!("{} ({:?})", r.name, r.verdict);
            let mut f = super::super::swap::tests::fake();
            f.st.seen_reassert_gen = 9;
            (r.setup)(&mut f);
            let ask = (r.ask)();
            let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
            let built = f.st.perform(plan, ask, |_, plan, _, _| {
                framed(script(r.script, plan, &keyframes))
            });
            assert!(matches!(built, Ok(Rebuilt::Built)), "{name}");
            assert_eq!(
                *f.st.shared.frame_map.lock().unwrap(),
                framing(),
                "{name}: frame map"
            );
            let told = heard(&mut f);
            assert_eq!(!told.is_empty(), r.told, "{name}: told");
            assert!(
                told.iter().all(|&(_, e, _)| e == (8, 8)),
                "{name}: {told:?}"
            );
        }
    }

    /// U5: a capture loss and a session switch rebuild at the view the session frames, and
    /// keep it after.
    #[test]
    fn the_view_survives_a_capture_loss_and_a_session_switch() {
        use punktfunk_core::video_fit::VideoFit;
        let view = Some((VideoFit::Fit, (8, 8)));
        let keyframes = Arc::new(AtomicU32::new(0));
        let lost: fn() -> RebuildAsk = || RebuildAsk::CaptureLost(None);
        for ask in [lost, switch] {
            let mut f = super::super::swap::tests::fake();
            frame_at(&mut f, view);
            let ask = ask();
            let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
            let mut opened_for = None;
            let built = f.st.perform(plan, ask, |st, plan, _, _| {
                opened_for = Some(
                    st.build_request(plan.intent, plan.rate_kbps)
                        .intent
                        .framing(),
                );
                script(Script::Pipeline(SMALL, 20_000), plan, &keyframes)
            });
            assert!(matches!(built, Ok(Rebuilt::Built)));
            assert_eq!(opened_for, Some(view), "the build opens framed");
            assert_eq!(
                f.st.live.geometry.intent().framing(),
                view,
                "the view stays"
            );
        }
    }

    /// U13: a switch into gamescope that runs the format rule for it again builds framed at the
    /// view, keeps it, and tells the new epoch at the size it encodes.
    #[test]
    fn the_view_survives_a_game_mode_switch() {
        use punktfunk_core::video_fit::VideoFit;
        let view = Some((VideoFit::Fit, (8, 8)));
        let keyframes = Arc::new(AtomicU32::new(0));
        let mut f = super::super::swap::tests::fake();
        frame_at(&mut f, view);
        f.st.facts.format_facts.follows = true;
        let ask = switch();
        let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
        let mut opened_for = None;
        let built = f.st.perform(plan, ask, |st, plan, _, _| {
            opened_for = Some(
                st.build_request(plan.intent, plan.rate_kbps)
                    .intent
                    .framing(),
            );
            framed(script(Script::Pipeline(SMALL, 20_000), plan, &keyframes))
        });
        assert!(matches!(built, Ok(Rebuilt::Built)));
        assert_eq!(f.st.display.compositor, Compositor::Gamescope);
        let framing = f.st.live.geometry.intent().framing();
        assert_eq!((opened_for, framing), (Some(view), view));
        let told: Vec<_> = heard(&mut f).into_iter().map(|(_, e, _)| e).collect();
        assert_eq!(told, [(8, 8)]);
    }

    /// U8: the owner of a display another session holds asks a smaller size, which is framed
    /// on the live mode and corrected; the control task takes the told view. A rate pick at the
    /// told size with no view, the ask a ring sends, keeps the framing, as one at the asked size
    /// does, and nothing builds.
    #[test]
    fn a_refresh_ask_on_a_shared_display_keeps_the_framed_view() {
        use crate::native::control::held_after;
        use crate::native::control::tests::control_ask;
        use crate::native::tests::Verdict;
        use punktfunk_core::quic::Reconfigure;
        use punktfunk_core::video_fit::VideoFit;
        let codec = crate::encode::Codec::H265;
        let req = |mode, view| Reconfigure {
            mode,
            exclude_codecs: 0,
            view,
        };
        let (live, small) = (mode((1920, 1080), 60), mode((1280, 720), 60));
        let framed_720 = Some((VideoFit::Fit, (1280, 720)));
        for (pick, view, verdict) in [
            (mode((1920, 1080), 120), Some((1280, 720)), Verdict::Wanted),
            (mode((1280, 720), 120), None, Verdict::Wanted),
        ] {
            let mut f = hd_session();
            let held = Intent::of(live, None);
            let (asked, held) = control_ask(req(small, None), held, codec, false, true);
            let asked = asked.expect("the owner may resize");
            let asked_at = Some(f.st.live.epoch);
            assert!(rebuild_with(
                &mut f,
                RebuildAsk::Mode(asked),
                true,
                Open::Frames
            ));
            assert_eq!(
                f.st.live.geometry.intent().framing(),
                framed_720,
                "framed on the live mode"
            );
            let told: Vec<_> = std::iter::from_fn(|| f.told.try_recv().ok()).collect();
            let last = told.last().expect("a correction");
            let p = last.picture.mode();
            let correction = (last.delivered, (p.width, p.height), last.corrects);
            assert_eq!(correction, (live, (1280, 720), true), "corrected");
            let (held, _) = held_after(held, asked_at, last);
            let (asked, _) = control_ask(req(pick, None), held, codec, false, true);
            let asked = asked.expect("a rate pick is accepted");
            let carried = (asked.mode(), asked.view());
            assert_eq!(carried, (pick, view), "{pick:?} ({verdict:?})");
            let built = rebuild_with(&mut f, RebuildAsk::Mode(asked), true, Open::Frames);
            assert!(!built, "{pick:?} ({verdict:?})");
            let framing = f.st.live.geometry.intent().framing();
            assert_eq!(framing, framed_720, "{pick:?} ({verdict:?})");
            let told = heard(&mut f);
            assert_eq!(
                told.last(),
                Some(&(live, (1280, 720), true)),
                "{pick:?} ({verdict:?})"
            );
        }
    }

    /// U9: the owner of a display another session is on changes codec only where the display
    /// stays as it is lit. One that would relight it is refused: nothing builds, the display and
    /// the plan stay, and the control task hears the codec kept. A content change is refused
    /// there too.
    #[test]
    fn an_owners_codec_change_under_a_joiner() {
        use crate::encode::Codec::H264;
        use crate::native::tests::Verdict;
        let verdict = Verdict::Wanted;
        // The display lit in HDR, which H.264 does not carry, then in SDR.
        for (lit, changes) in [(true, false), (false, true)] {
            let mut f = super::super::swap::tests::fake();
            f.st.display.vd_params.hdr = lit;
            let keyframes = Arc::new(AtomicU32::new(0));
            let facts = RebuildFacts {
                hold: Hold::Shared,
                ..f.st.rebuild_facts()
            };
            assert!(rebuild_plan::plan(&RebuildAsk::Content(false), &facts).is_none());
            let (ask, before) = (RebuildAsk::Format(H264), f.st.plan.codec);
            assert_ne!(before, H264);
            let plan = rebuild_plan::plan(&ask, &facts).expect("a codec rebuild");
            assert_eq!(plan.display, Some(DisplayChange::FormatKept(H264)));
            let built = f.st.perform(plan, ask, |_, plan, _, _| {
                script(Script::Pipeline(SMALL, 20_000), plan, &keyframes)
            });
            let built = matches!(built, Ok(Rebuilt::Built));
            assert_eq!(built, changes, "lit {lit} ({verdict:?})");
            let relit = f.display.lock().unwrap().hdr;
            assert_ne!(
                relit,
                Some(!lit),
                "lit {lit}: the display stays as it is lit"
            );
            let runs = if changes { H264 } else { before };
            assert_eq!(f.st.plan.codec, runs, "lit {lit}");
            let heard = f.codecs.try_recv().ok();
            assert_eq!(heard, Some(changes.then_some(H264)), "lit {lit}");
        }
    }

    /// U10: an owner's commit republishes what a later joiner reads off its row: the mode a
    /// switch built or a source moved to, the colour a codec change lit, the compositor, route
    /// and shared format a session switch moved to. A failed build leaves what runs.
    #[test]
    fn a_commit_republishes_what_joiners_read() {
        use crate::native::tests::Verdict::{self, Wanted};
        use crate::vdisplay::admission;
        // The ask, its build, whether a joiner then reads the mode that runs.
        type Row = (&'static str, fn() -> RebuildAsk, Script, bool, Verdict);
        let wide: fn() -> RebuildAsk = || RebuildAsk::Mode(ask(mode(WIDE, 60), None));
        let rows: [Row; 5] = [
            (
                "mode switch",
                wide,
                Script::Pipeline(WIDE, 20_000),
                true,
                Wanted,
            ),
            ("failed mode switch", wide, Script::Fails, true, Wanted),
            (
                "codec change",
                || RebuildAsk::Format(crate::encode::Codec::H264),
                Script::Pipeline(SMALL, 20_000),
                true,
                Wanted,
            ),
            (
                "session switch",
                switch,
                Script::Pipeline(SMALL, 20_000),
                true,
                Wanted,
            ),
            (
                "source mode",
                || RebuildAsk::SourceMode(DELIVERED),
                Script::Encoder,
                true,
                Wanted,
            ),
        ];
        for (name, ask, built, mode_told, verdict) in rows {
            let keyframes = Arc::new(AtomicU32::new(0));
            // The fake holds the registry's test lock, which every session that admits holds.
            let mut f = super::super::swap::tests::fake();
            f.st.display.vd_params.hdr = true;
            let sink = Arc::new(std::sync::Mutex::new(Some(name.to_string())));
            let owner = admission::LiveDisplay {
                compositor: Some(f.st.display.compositor),
                audio_sink: sink.clone(),
                hdr: true,
                ..Default::default()
            };
            let m = f.st.live.geometry.intent().mode();
            let stop = f.st.facts.life.stop_flag().clone();
            let at = (m.width, m.height, m.refresh_hz);
            let _row = admission::register(None, at, stop, name.into(), owner, None);
            let ask = ask();
            let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
            let _ =
                f.st.perform(plan, ask, |_, plan, _, _| script(built, plan, &keyframes));
            let joiner = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (joins_at, read) = admission::join_target(&joiner).expect("the owner's row");
            assert!(
                Arc::ptr_eq(&read.audio_sink, &sink),
                "{name}: the owner's row"
            );
            let (st, m) = (&f.st, f.st.live.geometry.intent().mode());
            let shared = admission::shares_capture(Some(st.display.compositor));
            let format = shared.then(|| admission::SampleFormat {
                bit_depth: st.bit_depth,
                chroma_444: st.plan.chroma.is_444(),
                full_range: st.plan.full_range(st.plan.hdr, st.bit_depth),
                hdr: st.plan.hdr,
            });
            let d = &st.display;
            let runs = (
                Some(d.compositor),
                d.vd_params.route.clone(),
                d.vd_params.hdr,
                format,
            );
            let reads = (read.compositor, read.route, read.hdr, read.format);
            assert_eq!(reads, runs, "{name}: the display a joiner reads");
            let ran = (m.width, m.height, m.refresh_hz);
            let told = joins_at == ran;
            assert_eq!(
                told, mode_told,
                "{name} ({verdict:?}): {joins_at:?} / {ran:?}"
            );
        }
    }

    /// U12: a mode switch runs no format rule at the new size: the depth and chroma stay under
    /// the client's decoder. A size the format does not take is the encoder's to refuse:
    /// PyroWave 4:4:4 past the rate controller's block range fails its open, and the session
    /// keeps the mode it ran.
    #[test]
    fn a_mode_switch_keeps_a_format_the_new_size_takes() {
        use crate::encode::{ChromaFormat, Codec};
        let verdict = crate::native::tests::Verdict::Wanted;
        let keyframes = Arc::new(AtomicU32::new(0));
        for (codec, to, opens) in [
            (Codec::H265, mode(WIDE, 60), true),
            (Codec::PyroWave, mode((7680, 4320), 60), false),
        ] {
            let mut f = super::super::swap::tests::fake();
            (f.st.plan.codec, f.st.plan.chroma) = (codec, ChromaFormat::Yuv444);
            (f.st.plan.bit_depth, f.st.bit_depth) = (10, 10);
            let ran = f.st.live.geometry.intent().mode();
            let ask = RebuildAsk::Mode(ask(to, None));
            let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
            assert_eq!(plan.display, None, "{codec:?} ({verdict:?})");
            let built = f.st.perform(plan, ask, |_, plan, _, _| {
                let open = if opens {
                    Script::Pipeline(WIDE, 20_000)
                } else {
                    Script::Fails
                };
                script(open, plan, &keyframes)
            });
            assert_eq!(matches!(built, Ok(Rebuilt::Built)), opens, "{codec:?}");
            let runs = f.st.live.geometry.intent().mode();
            assert_eq!(
                runs == ran,
                !opens,
                "{codec:?}: a refused size keeps the mode"
            );
            let kept = (f.st.plan.chroma, f.st.plan.bit_depth, f.st.bit_depth);
            assert_eq!(
                kept,
                (ChromaFormat::Yuv444, 10, 10),
                "{codec:?} ({verdict:?})"
            );
        }
    }

    /// U17: a mode build that loses its capture keeps the mode and view that ran and corrects
    /// the client. The capture loss after it rebuilds at that intent, the one the client was
    /// told, not at the one it asked.
    #[test]
    fn a_loss_inside_a_mode_build_keeps_the_intent_that_ran() {
        use crate::native::tests::Verdict;
        use punktfunk_core::video_fit::VideoFit;
        let verdict = Verdict::Wanted;
        let view = Some((VideoFit::Fit, (8, 8)));
        let keyframes = Arc::new(AtomicU32::new(0));
        let mut f = super::super::swap::tests::fake();
        frame_at(&mut f, view);
        let ask = RebuildAsk::Mode(ask(mode(WIDE, 60), None));
        let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
        let kept = f.st.perform(plan, ask, |_, plan, _, _| {
            script(Script::Fails, plan, &keyframes)
        });
        assert!(matches!(kept, Ok(Rebuilt::Kept(_))));
        let kept = f.st.live.geometry.intent();
        assert_eq!((kept.mode(), kept.framing()), (ASKED, view));
        let told: Vec<_> = heard(&mut f).into_iter().map(|(m, _, c)| (m, c)).collect();
        assert_eq!(told, [(DELIVERED, true)], "{verdict:?}");
        let ask = RebuildAsk::CaptureLost(None);
        let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
        let mut retried = None;
        let built = f.st.perform(plan, ask, |st, plan, _, _| {
            let req = st.build_request(plan.intent, plan.rate_kbps);
            retried = Some((req.intent.mode(), req.intent.framing()));
            script(Script::Pipeline(SMALL, 20_000), plan, &keyframes)
        });
        assert!(matches!(built, Ok(Rebuilt::Built)));
        assert_eq!(retried, Some((ASKED, view)), "{verdict:?}");
    }

    use crate::native::geometry::tests::Case;

    /// The fake on `case`'s display: whether the control task holds it, off the tenure cell,
    /// and the facts the rebuild plan reads, with another session on an owned display.
    fn on_display(f: &mut Session, case: Case) -> (bool, RebuildFacts) {
        let (tenure, generation, shared) = case.held();
        f.st.display.tenure = tenure;
        f.st.display.vd_params.join_live = tenure.join_live();
        f.st.live.cur_display_gen = generation;
        f.st.plan.mirrored = tenure.mirrors();
        f.st.shared.tenure.adopt(&tenure, generation);
        let facts = f.st.rebuild_facts();
        let facts = RebuildFacts {
            hold: if shared { Hold::Shared } else { facts.hold },
            ..facts
        };
        let held = f.st.shared.tenure.held(crate::vdisplay::registry::shared);
        (held, facts)
    }

    /// What the stream decides on `case`'s display: whether content turned HDR rebuilds, how a
    /// larger mode builds and whether it stops joining, and the capture the plan picks.
    pub(in crate::native) fn tenure_plans(
        case: Case,
    ) -> (
        bool,
        Option<(Build, bool)>,
        crate::session_plan::CaptureBackend,
    ) {
        use crate::native::handshake::tests::{hello_at, terms_for};
        let mut f = super::super::swap::tests::fake();
        let (_, facts) = on_display(&mut f, case);
        let tenure = f.st.display.tenure;
        let content = rebuild_plan::plan(&RebuildAsk::Content(true), &facts).is_some();
        let larger = rebuild_plan::plan(&RebuildAsk::Mode(ask(mode(WIDE, 60), None)), &facts)
            .map(|p| (p.build, p.stops_joining));
        let terms = terms_for(hello_at(ASKED), f.st.facts.terms.encoder.clone());
        let capture = super::super::pipeline::plan_for(
            &terms,
            crate::encode::Codec::H265,
            &terms.verdict,
            &tenure,
            Compositor::Mutter,
            None,
        )
        .capture;
        (content, larger, capture)
    }

    /// U11: a joiner's capture loss rebuilds at the live mode, held or alone, and nothing turns
    /// its join off: the reopen joins the owner's display where one runs, else builds one of its
    /// own (pf-vdisplay's pool, past this fake). It stays a joiner, and the control task's cell
    /// takes the display the build opened.
    #[test]
    fn a_joiners_capture_loss_joins_again_or_builds_its_own() {
        use crate::native::tests::Verdict;
        let verdict = Verdict::Wanted;
        let keyframes = Arc::new(AtomicU32::new(0));
        for case in [Case::JoinerHeld, Case::JoinerAlone] {
            let mut f = super::super::swap::tests::fake();
            let (_, facts) = on_display(&mut f, case);
            let ask = RebuildAsk::CaptureLost(None);
            let plan = rebuild_plan::plan(&ask, &facts).expect("a loss rebuild");
            let planned = (plan.build.clone(), plan.intent.mode(), plan.stops_joining);
            assert_eq!(planned, (Build::CaptureLoss, ASKED, false), "{case:?}");
            let mut joins = None;
            let built = f.st.perform(plan, ask, |st, plan, _, _| {
                joins = Some(st.display.vd_params.join_live);
                // The scripted pipeline is on display generation 2.
                script(Script::Pipeline(SMALL, 20_000), plan, &keyframes)
            });
            assert!(matches!(built, Ok(Rebuilt::Built)), "{case:?}");
            assert_eq!(
                joins,
                Some(true),
                "{case:?} ({verdict:?}): the reopen joins"
            );
            assert_eq!(
                f.display.lock().unwrap().join_live,
                None,
                "{case:?}: join kept"
            );
            assert!(f.st.display.tenure.join_live(), "{case:?}: still a joiner");
            let cell = &f.st.shared.tenure;
            assert!(
                cell.held(|g| g == 2),
                "{case:?}: held while another is on it"
            );
            let held_alone = cell.held(|_| false);
            assert_eq!(held_alone, !cfg!(target_os = "linux"), "{case:?}: alone");
        }
    }

    /// An encoder on a `size` capture that frames on ingest, opened for `view` as a pipeline
    /// build opens it, and the framing it encodes.
    fn fitted(
        size: (u32, u32),
        view: Option<(punktfunk_core::video_fit::VideoFit, (u32, u32))>,
    ) -> Result<(
        Box<dyn crate::encode::Encoder>,
        punktfunk_core::video_fit::Reframe,
    )> {
        crate::session_plan::open_encoder_fitted(size, view, |_| {
            let crop = Default::default();
            Ok(Box::new(crate::session_plan::tests::Frames(true, crop)))
        })
    }

    /// A full build at the plan's mode, framed for the view `prepare` set.
    fn display_built(st: &StreamState, plan: &RebuildPlan) -> Result<Built> {
        let size = (plan.intent.mode().width, plan.intent.mode().height);
        let (enc, reframe) = fitted(size, plan.intent.framing())?;
        let interval = plan.intent.mode().frame_interval();
        let pipeline = Pipeline {
            capturer: Box::new(crate::capture::SyntheticCapturer::new(size.0, size.1, 60)),
            enc,
            frame: frame(size),
            geometry: Geometry::of(plan.intent, size, interval, reframe),
            node_id: 2,
            display_gen: st.live.cur_display_gen,
            bitrate_kbps: plan.rate_kbps,
            #[cfg(target_os = "linux")]
            lease: None,
        };
        Ok(Built::pipeline(pipeline, None))
    }

    /// A vector's `[w, h, hz]` mode.
    fn vector_mode(v: &serde_json::Value) -> punktfunk_core::Mode {
        let n = |i: usize| v[i].as_u64().expect("a whole number") as u32;
        mode((n(0), n(1)), n(2))
    }

    /// A vector's `[w, h]` size, `None` for `null`.
    fn vector_size(v: &serde_json::Value) -> Option<(u32, u32)> {
        let a = v.as_array()?;
        let n = |i: usize| a[i].as_u64().expect("a whole number") as u32;
        Some((n(0), n(1)))
    }

    /// `row` run through the host's functions: the control task's answer to the ask, then the
    /// rebuild it plans and performs on the fake. Its `build`, `kept` and `told`, as the
    /// vector file spells them.
    fn vector_run(row: &serde_json::Value) -> serde_json::Value {
        use crate::native::control::tests::control_ask;
        use punktfunk_core::quic::Reconfigure;
        use punktfunk_core::video_fit::VideoFit;
        use serde_json::json;
        let tenure = match row["tenure"].as_str() {
            Some("own") => Case::Own,
            Some("shared") => Case::OwnShared,
            Some("joiner-held") => Case::JoinerHeld,
            Some("joiner-alone") => Case::JoinerAlone,
            t => panic!("tenure {t:?}"),
        };
        let mut f = hd_session();
        if row["codec"] == "pyrowave" {
            pyrowave(&mut f, false);
        }
        let live = vector_mode(&row["live"]["mode"]);
        let size = (live.width, live.height);
        f.st.live.frame = frame(size);
        let view = vector_size(&row["live"]["view"]).map(|v| (VideoFit::Fit, v));
        let framing = fitted(size, view).expect("the live framing").1;
        running(
            &mut f,
            Intent::of(live, view),
            live.frame_interval(),
            framing,
        );
        let (held, facts) = on_display(&mut f, tenure);
        let req = Reconfigure {
            mode: vector_mode(&row["ask"]["mode"]),
            exclude_codecs: 0,
            view: vector_size(&row["ask"]["view"]),
        };
        let hold = ask(
            vector_mode(&row["hold"]["mode"]),
            vector_size(&row["hold"]["view"]),
        );
        let exact = crate::native::handshake::runs_exact_rate(Some(Compositor::Mutter), false);
        let (asked, _) = control_ask(req, hold, f.st.plan.codec, exact, !held);
        let build = match asked {
            None => "refused",
            Some(asked) => match rebuild_plan::plan(&RebuildAsk::Mode(asked), &facts) {
                None => {
                    f.st.tell(f.st.live.geometry.delivered() != asked.mode());
                    "none"
                }
                Some(plan) => {
                    let encoder = plan.build == Build::Encoder;
                    let opened = std::sync::Mutex::new(Vec::new());
                    let built =
                        f.st.perform(plan, RebuildAsk::Mode(asked), |st, plan, _, _| {
                            if encoder {
                                st.build_encoder(plan, opener(Open::Frames, size, &opened))
                            } else {
                                display_built(st, plan)
                            }
                        });
                    assert!(matches!(built, Ok(Rebuilt::Built)), "{}", row["name"]);
                    if encoder {
                        "encoder"
                    } else {
                        "display"
                    }
                }
            },
        };
        let told = heard(&mut f).last().map_or(serde_json::Value::Null, |&(m, e, c)| {
            json!({"mode": [m.width, m.height, m.refresh_hz], "picture": [e.0, e.1], "corrects": c})
        });
        let kept = f.st.live.geometry.intent();
        let (m, framing) = (kept.mode(), kept.framing());
        json!({
            "build": build,
            "kept": {
                "mode": [m.width, m.height, m.refresh_hz],
                "view": framing.map(|(_, v)| [v.0, v.1]),
            },
            "told": told,
        })
    }

    /// A Hello row: the Welcome a joiner gets on a display of the `owner` mode, Automatic.
    fn vector_hello(row: &serde_json::Value) -> serde_json::Value {
        use crate::native::handshake::tests::{welcomed, Welcomed};
        let codec = match row["codec"].as_str() {
            Some("pyrowave") => crate::encode::Codec::PyroWave,
            _ => crate::encode::Codec::H265,
        };
        let (asked, view) = (
            vector_mode(&row["hello"]["mode"]),
            vector_size(&row["hello"]["view"]),
        );
        let owner = Welcomed::Joiner(vector_mode(&row["owner"]));
        let (m, kbps, picture) = welcomed(asked, view, codec, 0, owner);
        serde_json::json!({
            "mode": [m.width, m.height, m.refresh_hz],
            "kbps": kbps,
            "picture": picture.map(|(w, h)| [w, h]),
        })
    }

    /// The mode-and-view ask rule, rules 1 to 4, as `clients/shared/mode-view-vectors.json`
    /// pins it for every reader.
    #[test]
    fn mode_view_vectors() {
        let raw = include_str!("../../../../../../clients/shared/mode-view-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        let pyrowave_runs =
            crate::encode::validate_dimensions(crate::encode::Codec::PyroWave, 1920, 1080).is_ok();
        for row in file["rows"].as_array().expect("rows") {
            let name = format!("{} ({})", row["name"], row["verdict"]);
            let host_skips = match row["only"].as_str() {
                Some("linux") => !cfg!(target_os = "linux"),
                Some("windows") => !cfg!(target_os = "windows"),
                _ => false,
            };
            if host_skips || (row["codec"] == "pyrowave" && !pyrowave_runs) {
                continue;
            }
            if row["at"] == "hello" {
                assert_eq!(vector_hello(row), row["welcome"], "{name}");
                continue;
            }
            let want = serde_json::json!({
                "build": row["build"],
                "kept": row["kept"],
                "told": row["told"],
            });
            assert_eq!(vector_run(row), want, "{name}");
        }
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

    /// U6: a swap installs the geometry its build returned and writes the input plane's frame
    /// map from it, for a whole pipeline and for an encoder alone.
    #[test]
    fn a_swap_writes_the_frame_map_it_encodes() {
        let verdict = crate::native::tests::Verdict::Wanted;
        let keyframes = Arc::new(AtomicU32::new(0));
        let view_ask = || RebuildAsk::Mode(ask(ASKED, Some((8, 8))));
        for (ask, built) in [
            (
                RebuildAsk::CaptureLost(None),
                Script::Pipeline(SMALL, 20_000),
            ),
            (view_ask(), Script::Encoder),
        ] {
            let mut f = super::super::swap::tests::fake();
            let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
            let built = framed(script(built, &plan, &keyframes)).expect("a build");
            let installs = match &built.next {
                Next::Pipeline(p) => p.geometry,
                Next::Encoder { geometry, .. } => *geometry,
            };
            f.st.swap_pipeline(built.next, plan.reason);
            let name = format!("{:?} ({verdict:?})", plan.reason);
            assert_eq!(f.st.live.geometry, installs, "{name}");
            let map = *f.st.shared.frame_map.lock().unwrap();
            assert_eq!((map, installs.reframe()), (framing(), framing()), "{name}");
        }
    }

    /// A mirror fits its head to the client's size. A source that delivers another mode keeps
    /// that framing, and the capture loss after it rebuilds framed at the client's size.
    #[test]
    fn a_mirror_keeps_the_clients_size_across_its_sources_mode() {
        use punktfunk_core::video_fit::VideoFit;
        let verdict = crate::native::tests::Verdict::Wanted;
        let keyframes = Arc::new(AtomicU32::new(0));
        let mut f = super::super::swap::tests::fake();
        let mirror = Tenure::of(None, true);
        (f.st.display.tenure, f.st.plan.mirrored) = (mirror, true);
        let g = f.st.live.geometry;
        let at = Intent::at_hello(g.intent().mode(), None, &mirror, f.st.plan.codec);
        running(&mut f, at, g.interval(), g.reframe());
        let fit = Some((VideoFit::Fit, SMALL));
        assert_eq!(at.framing(), fit, "fitted to the client's size");
        f.st.live.frame = frame(WIDE);
        let ask = RebuildAsk::SourceMode(DELIVERED_WIDE);
        let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("an encoder rebuild");
        let opened = std::sync::Mutex::new(Vec::new());
        let built = f.st.perform(plan, ask, |st, plan, _, _| {
            st.build_encoder(plan, opener(Open::Frames, WIDE, &opened))
        });
        assert!(matches!(built, Ok(Rebuilt::Built)));
        let g = f.st.live.geometry;
        let after = (g.intent().framing(), g.delivered(), g.encoded());
        let fitted = punktfunk_core::video_fit::Reframe::plan(VideoFit::Fit, WIDE, SMALL).out;
        assert_eq!(after, (fit, DELIVERED_WIDE, fitted), "{verdict:?}");
        let ask = RebuildAsk::CaptureLost(None);
        let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a loss rebuild");
        let mut retried = None;
        let built = f.st.perform(plan, ask, |st, plan, _, _| {
            retried = Some(
                st.build_request(plan.intent, plan.rate_kbps)
                    .intent
                    .framing(),
            );
            script(Script::Pipeline(WIDE, 20_000), plan, &keyframes)
        });
        assert!(matches!(built, Ok(Rebuilt::Built)));
        assert_eq!(retried, Some(fit), "{verdict:?}");
    }

    /// An IDD-push monitor as the in-place resize sees it: a mode set and a presentation
    /// restart each answer `restores`, then it delivers `frames`, oldest first.
    struct IddMonitor {
        restores: bool,
        frames: VecDeque<crate::capture::CapturedFrame>,
    }

    impl crate::capture::Capturer for IddMonitor {
        fn next_frame(
            &mut self,
            _: pf_capture::FirstFrame,
        ) -> Result<crate::capture::CapturedFrame> {
            anyhow::bail!("the resize takes frames as they come")
        }
        fn try_latest(&mut self) -> Result<Option<crate::capture::CapturedFrame>> {
            Ok(self.frames.pop_front())
        }
        fn display(&mut self) -> Option<&mut dyn pf_capture::DisplayControl> {
            Some(self)
        }
    }

    impl pf_capture::DisplayControl for IddMonitor {
        fn target_id(&self) -> u32 {
            1
        }
        fn resize(&mut self, _: u32, _: u32) -> bool {
            self.restores
        }
        fn restart_presentation(&mut self) -> bool {
            self.restores
        }
    }

    /// A frame of `size` the source composed as its `seq`th.
    fn composed(size: (u32, u32), seq: u64) -> crate::capture::CapturedFrame {
        crate::capture::CapturedFrame {
            provenance: pf_frame::Provenance::source(seq, 0),
            ..frame(size)
        }
    }

    /// The in-place resize past its mode set on the fake, for `plan` on a display that reads
    /// back 60 Hz, over `monitor`, its encoder opened as `how`: the geometry it swaps in and
    /// the rate it runs, or `None` for the full build. The driver's own open is Windows', so
    /// the fake's encoder opens on the host.
    fn resized(
        f: &mut Session,
        plan: &RebuildPlan,
        monitor: IddMonitor,
        recover_ring: bool,
        how: Open,
    ) -> Option<(Geometry, u32)> {
        let size = monitor.frames.back().map_or(SMALL, |m| (m.width, m.height));
        let portal = crate::session_plan::CaptureBackend::Portal;
        (f.st.live.capturer, f.st.plan.capture) = (Box::new(monitor), portal);
        let trace = Trace::start("resize-test", Arc::new(AtomicU32::new(0)));
        let (built, price) = (
            Geometry::built(plan.intent, (60, 60_000)),
            f.st.pricing(plan),
        );
        let opened = std::sync::Mutex::new(Vec::new());
        let open = opener(how, size, &opened);
        match f.st.resize_on(built, &price, &trace, recover_ring, open)? {
            Next::Encoder { geometry, .. } => Some((geometry, price(&geometry))),
            Next::Pipeline(_) => unreachable!("an in-place resize opens the encoder alone"),
        }
    }

    /// U1: an in-place resize delivers the size it mode-set at the rate the display runs,
    /// framed for the intent's view where the encoder takes it and at the capture where the
    /// framed open fails, and PyroWave runs at the picture it encodes.
    #[test]
    fn an_in_place_resize_frames_and_prices_what_it_opened() {
        use crate::native::bitrate::pyrowave_mode_kbps;
        use crate::native::geometry::Picture;
        use punktfunk_core::video_fit::VideoFit;
        let verdict = crate::native::tests::Verdict::Wanted;
        let (qhd, view) = (mode((2560, 1440), 60), (1280, 720));
        for (how, encoded) in [(Open::Frames, view), (Open::FailsFramed, (2560, 1440))] {
            let mut f = hd_session();
            pyrowave(&mut f, false);
            let ask = RebuildAsk::Mode(ask(qhd, Some(view)));
            let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a rebuild");
            let monitor = IddMonitor {
                restores: true,
                frames: [frame((2560, 1440))].into(),
            };
            let (g, rate) = resized(&mut f, &plan, monitor, false, how).expect("in place");
            let name = format!("{how:?} ({verdict:?})");
            let at = |(width, height)| {
                Picture::of(punktfunk_core::Mode {
                    width,
                    height,
                    ..qhd
                })
            };
            let framing = g.intent().framing();
            assert_eq!(
                (g.delivered(), framing),
                (qhd, Some((VideoFit::Fit, view))),
                "{name}"
            );
            assert_eq!((g.encoded(), g.picture()), (encoded, at(encoded)), "{name}");
            let priced = pyrowave_mode_kbps(None, &at(encoded), &f.st.plan, None);
            assert_eq!(rate, priced, "{name}");
        }
    }

    /// U2: a topology re-assert in place restores the presentation at the mode that runs and
    /// waits for a newer present: it keeps the live intent, size and framing, and falls back to
    /// the full build when the presentation does not come back.
    #[test]
    fn a_reassert_in_place_keeps_the_geometry_or_falls_back() {
        use punktfunk_core::video_fit::VideoFit;
        let verdict = crate::native::tests::Verdict::Wanted;
        for restores in [true, false] {
            let mut f = super::super::swap::tests::fake();
            frame_at(&mut f, Some((VideoFit::Fit, (8, 8))));
            idd_push(&mut f);
            let ask = RebuildAsk::TopologyReassert(5);
            let plan = rebuild_plan::plan(&ask, &f.st.rebuild_facts()).expect("a re-assert");
            let live = f.st.live.geometry.intent();
            let monitor = IddMonitor {
                restores,
                frames: [composed(SMALL, 5), composed(SMALL, 6)].into(),
            };
            let got = resized(&mut f, &plan, monitor, true, Open::Frames);
            let size = |g: &Geometry| (g.delivered().width, g.delivered().height);
            let kept = got.map(|(g, _)| (g.intent(), size(&g), g.encoded()));
            let fitted = punktfunk_core::video_fit::Reframe::plan(VideoFit::Fit, SMALL, (8, 8));
            let want = restores.then_some((live, SMALL, fitted.out));
            assert_eq!(kept, want, "restores {restores} ({verdict:?})");
        }
    }
}
