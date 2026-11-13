//! [`RemoteSource`]: the capture source whose encoder runs in another process. The driver's
//! (IDD-push) and a capture worker's (WGC) are one type over their [`Transport`].
//!
//! A delivery carries no pixels: it is the news that the far side's pool took a frame, read
//! off the encoder's own counter. One tick runs, in order, a decided fault, the far side's
//! news, the supervisor, the delivery gate and the delivery; IDD-push's wrapper composes the
//! same phases around its display, cursor and stall shells. The supervisor walks the recovery
//! ladder for the driver and only classifies the worker's mirror. Evidence:
//! `design/pf-capture-overhaul.md` §4.4.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use pf_frame::health::EncoderTelemetry;
use pf_frame::recovery::{Stage, StageOutcome};
use pf_frame::{CapturedFrame, PixelFormat};

use super::au_assembly::Clocks;
use super::delivery::{self, DeliveryGate};
use super::encoder;
use super::supervisor::{Inputs, Step, Supervision, Supervisor};
use super::transport::{Transport, TransportEvent};
use crate::{
    CaptureFault, CaptureHealth, Capturer, ColourFacts, DriverEncodeParams, EncoderSite,
    FirstFrame, RemoteEncoder, SourceEvent, Supervised,
};

/// How long a first delivery may take: the far side opens no encoder before the loop asks.
const FIRST_FRAME: Duration = Duration::from_secs(20);

/// The geometry a delivery carries; a change re-opens the loop's encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

/// What the source's owner samples for the supervisor: IDD-push's cursor witness, its
/// presentation restart in flight, the topology lock. Zero for the worker.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Local {
    pub cursor_gap_px: u32,
    pub recreating: bool,
    pub topology_held: bool,
}

/// The rungs the source's owner runs on the capture thread. The loop owns the encoder reset
/// and the driver cycle; these are the rest.
pub(crate) trait Rungs {
    /// Present the compose canary. `false`: nobody could see it.
    fn canary(&mut self) -> bool;
    /// Run `PresentationReset` or `SwapChainReset`.
    fn run(&mut self, stage: Stage) -> StageOutcome;
}

/// No local actuator: the canary is never shown, every local rung is `Unsupported`.
pub(crate) struct NoRungs;

impl Rungs for NoRungs {
    fn canary(&mut self) -> bool {
        false
    }

    fn run(&mut self, _stage: Stage) -> StageOutcome {
        StageOutcome::Unsupported
    }
}

/// A source whose encoder runs in another process: the driver's (IDD-push) or a capture
/// worker's (WGC). A delivery is the news that the encoder's pool took a frame.
pub(crate) struct RemoteSource<T: Transport> {
    transport: T,
    clocks: Clocks,
    /// Freshness over the encoder's pool counter and the delivered geometry.
    gate: DeliveryGate<Geometry>,
    /// The last admitted geometry; the worker's resize moves it.
    geometry: Geometry,
    supervisor: Supervisor,
    /// The session encoder's clocks from the loop's last `observe`.
    telemetry: Option<EncoderTelemetry>,
    /// The last delivery's sequence, and when.
    seq: u64,
    last_fresh: Instant,
    /// Frames the pool took plus dropped, and when that last moved: the ladder's source clock,
    /// since takes alone freeze while the driver's worker is healthy.
    drain_seq: u64,
    last_drain: Instant,
    /// What the loop has not taken yet: a rung, a closed episode's outage, a typed end.
    pending_stage: Option<Stage>,
    recovered_outage: Option<Duration>,
    pending_fault: Option<anyhow::Error>,
    /// Reaches whichever encoder this source opened last.
    poke: Arc<encoder::Poke>,
    /// Held for the source's life, dropped last.
    _keepalive: Box<dyn Send>,
}

impl<T: Transport> RemoteSource<T> {
    pub(crate) fn new(
        transport: T,
        geometry: Geometry,
        supervision: Supervision,
        clocks: Clocks,
        keepalive: Box<dyn Send>,
    ) -> Self {
        let now = (clocks.now)();
        Self {
            transport,
            clocks,
            gate: DeliveryGate::new(),
            geometry,
            supervisor: Supervisor::new(now, supervision),
            telemetry: None,
            seq: 0,
            last_fresh: now,
            drain_seq: 0,
            last_drain: now,
            pending_stage: None,
            recovered_outage: None,
            pending_fault: None,
            poke: Arc::default(),
            _keepalive: keepalive,
        }
    }

    /// One tick with no owner extras, in the order IDD-push's wrapper composes the same
    /// phases. `geometry` is the owner's display; `None` delivers the far side's size, read
    /// after its news. `Ok(None)` means the far side composed nothing new.
    pub(crate) fn tick(
        &mut self,
        geometry: Option<Geometry>,
        local: Local,
        rungs: &mut dyn Rungs,
    ) -> Result<Option<CapturedFrame>> {
        self.take_fault()?;
        if self.poll()? {
            return Ok(None);
        }
        self.supervise(local, rungs)?;
        if !self.admit(geometry.unwrap_or(self.geometry)) {
            return Ok(None);
        }
        Ok(Some(self.deliver((self.clocks.now)())))
    }

    /// The typed end the ladder or a stage report decided off the capture path.
    pub(crate) fn take_fault(&mut self) -> Result<()> {
        self.pending_fault.take().map_or(Ok(()), Err)
    }

    /// Apply the far side's news. `true`: a rung was parked and the tick ends here.
    pub(crate) fn poll(&mut self) -> Result<bool> {
        let now = (self.clocks.now)();
        for event in self.transport.poll(now, self.last_fresh)? {
            match event {
                TransportEvent::Resized { width, height } => {
                    (self.geometry.width, self.geometry.height) = (width, height);
                }
                TransportEvent::DriverGone => {
                    self.pending_stage = Some(Stage::DriverCycle);
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Staged recovery (immunity plan WP12/WP13). A wedged-but-ALIVE display answers
    /// `Ok(None)` forever, so a known-active desktop could stream nothing indefinitely. The
    /// classifier names the gap from the far side's own clocks — drain heartbeat, pool source
    /// sequence, access units — plus `local`'s cursor travel and an unanswered canary; the
    /// coordinator walks the ladder from that class, one rung at a time under a deadline, and
    /// proves each rung with NEW source frames (access units for an encoder stall). No
    /// evidence = plain idle = no recovery. An exhausted ladder ends the plane with the typed
    /// `CaptureFault::SourceStalled`.
    pub(crate) fn supervise(&mut self, local: Local, rungs: &mut dyn Rungs) -> Result<()> {
        let now = (self.clocks.now)();
        let drain = self.telemetry.map_or(0, |t| t.drain_progress());
        if drain != self.drain_seq {
            self.drain_seq = drain;
            self.last_drain = now;
        }
        let inputs = Inputs {
            now,
            // The ladder reads the drain clock; the classify-only mirror its own deliveries.
            last_source: if self.supervisor.ladder() {
                self.last_drain
            } else {
                self.last_fresh
            },
            source_seq: self.drain_seq,
            heartbeat_age: self.heartbeat_age(),
            cursor_gap_px: local.cursor_gap_px,
            recreating: local.recreating,
            // The OS's verdict: UAC holds the ladder in every session.
            secure_desktop: self.transport.secure_desktop(),
            topology_held: local.topology_held,
            encoder: self.telemetry,
        };
        let step = self.supervisor.tick(inputs);
        self.drive(step, rungs)
    }

    /// Whether this tick delivers. The encoder's open time and pool counter are the only source
    /// clock this side has; the rule is [`DeliveryGate::admit`].
    pub(crate) fn admit(&mut self, geometry: Geometry) -> bool {
        let source = self.telemetry.map(|t| (t.opened_at, t.source_seq));
        if !self.gate.admit(source, geometry) {
            return false;
        }
        self.geometry = geometry;
        true
    }

    /// The delivery [`Self::admit`] let through. A recovery episode closes only on the
    /// budgeted count of NEW source frames.
    pub(crate) fn deliver(&mut self, now: Instant) -> CapturedFrame {
        if let Some((summary, outage)) = self.supervisor.source_frame(now) {
            tracing::info!(
                target = %self.transport.name(),
                ?summary,
                outage_ms = outage.as_millis() as u64,
                "IDD push: recovery episode closed"
            );
            self.recovered_outage = Some(outage);
        }
        self.last_fresh = now;
        self.seq += 1;
        let Geometry {
            width,
            height,
            format,
        } = self.geometry;
        delivery::frame(self.seq, width, height, format)
    }

    /// A loop-owned rung finished with `outcome`; the ladder's next step runs here. A driver
    /// cycle, applied or not, ends the source: the WUDFHost its encoder points at is gone
    /// either way, and the driver-death watch fires the cycle outside any episode, so nothing
    /// else bounds a reload that keeps failing.
    pub(crate) fn stage_done_with(
        &mut self,
        stage: Stage,
        outcome: StageOutcome,
        rungs: &mut dyn Rungs,
    ) {
        let step = self.finish_stage(stage, outcome);
        if let Err(e) = self.drive(step, rungs) {
            self.pending_fault = Some(e);
            return;
        }
        if matches!(stage, Stage::DriverCycle) {
            let applied = matches!(outcome, StageOutcome::Applied);
            self.pending_fault = Some(anyhow::anyhow!(
                "IDD-push: the pf-vdisplay driver cycle {} — ending the capturer so the session \
                 rebuilds its virtual output",
                if applied {
                    "reloaded the adapter"
                } else {
                    "FAILED (adapter not reloaded)"
                }
            ));
        }
    }

    /// How long ago the far side's drain worker last finished a pass; `None` before its first
    /// heartbeat (no encoder open yet).
    pub(crate) fn heartbeat_age(&self) -> Option<Duration> {
        let t = self.telemetry?;
        Some(
            (self.clocks.now)()
                .checked_duration_since(t.drain_heartbeat?)
                .unwrap_or_default(),
        )
    }

    pub(crate) fn telemetry(&self) -> Option<EncoderTelemetry> {
        self.telemetry
    }

    /// An episode is open: the owner's own recover-or-drop timers stand down.
    pub(crate) fn owns_episode(&self) -> bool {
        self.supervisor.owns_episode()
    }

    /// Walk the supervisor's steps until it rests or the plane ends. A rung the stream loop
    /// owns is parked for [`Capturer::events`]; the loop's `stage_done` re-enters here with
    /// its outcome. The rest run in `rungs`; a rung with no actuator on this host reports
    /// `Unsupported` and the ladder moves on unpenalised.
    fn drive(&mut self, mut step: Step, rungs: &mut dyn Rungs) -> Result<()> {
        loop {
            match step {
                Step::Nothing => return Ok(()),
                Step::Canary => {
                    // A canary nobody saw proves nothing: it must not age into strong evidence.
                    if !rungs.canary() {
                        self.supervisor.canary_not_shown();
                    }
                    return Ok(());
                }
                Step::Run(stage @ (Stage::EncoderReset | Stage::DriverCycle)) => {
                    self.pending_stage = Some(stage);
                    return Ok(());
                }
                Step::Run(stage) => {
                    let outcome = rungs.run(stage);
                    step = self.finish_stage(stage, outcome);
                }
                Step::Recovered { summary, outage } => {
                    tracing::info!(
                        target = %self.transport.name(),
                        ?summary,
                        outage_ms = outage.as_millis() as u64,
                        "IDD push: recovery episode closed"
                    );
                    self.recovered_outage = Some(outage);
                    return Ok(());
                }
                Step::Failed { gap, summary } => {
                    let fault = CaptureFault::SourceStalled {
                        secs: gap.as_secs() as u32,
                    };
                    tracing::error!(
                        target = %self.transport.name(),
                        %fault,
                        ?summary,
                        "IDD push: recovery ladder exhausted"
                    );
                    return Err(anyhow::Error::new(fault).context(
                        "IDD-push: a known-active display delivered no source frame through the \
                         recovery ladder — ending the video plane with a typed error",
                    ));
                }
            }
        }
    }

    /// Record a rung's outcome with the supervisor and take its next step.
    fn finish_stage(&mut self, stage: Stage, outcome: StageOutcome) -> Step {
        tracing::warn!(
            target = %self.transport.name(),
            ?stage,
            ?outcome,
            "IDD push: recovery stage"
        );
        self.supervisor
            .stage_done((self.clocks.now)(), stage, outcome)
    }
}

/// Tick until a delivery, at most [`FIRST_FRAME`], 4 ms apart; past it, the `late` error.
pub(crate) fn first_frame(
    mut tick: impl FnMut() -> Result<Option<CapturedFrame>>,
    late: impl FnOnce() -> anyhow::Error,
) -> Result<CapturedFrame> {
    let deadline = Instant::now() + FIRST_FRAME;
    loop {
        if let Some(f) = tick()? {
            return Ok(f);
        }
        if Instant::now() > deadline {
            return Err(late());
        }
        std::thread::sleep(Duration::from_millis(4));
    }
}

impl<T: Transport> Capturer for RemoteSource<T> {
    fn next_frame(&mut self, _budget: FirstFrame) -> Result<CapturedFrame> {
        let name = self.transport.name().to_owned();
        first_frame(
            || self.try_latest(),
            || anyhow!("no frame of {name} within 20s — the capture worker's pool took none"),
        )
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.tick(None, Local::default(), &mut NoRungs)
    }

    fn is_alive(&self) -> bool {
        self.transport.alive()
    }

    fn events(&mut self) -> Vec<SourceEvent> {
        let stage = self.pending_stage.take().map(SourceEvent::RunStage);
        let outage = self
            .recovered_outage
            .take()
            .map(SourceEvent::RecoveredOutage);
        stage.into_iter().chain(outage).collect()
    }

    /// BT.2020 PQ while the delivery is P010 (an FP16 capture), with the generic HDR10 volume:
    /// neither far side forwards the source's own.
    fn colour(&self) -> ColourFacts {
        ColourFacts {
            hdr_meta: (self.geometry.format == PixelFormat::P010)
                .then(pf_frame::hdr::generic_hdr10),
            sdr_fp16: false,
        }
    }

    fn supervised(&mut self) -> Option<&mut dyn Supervised> {
        Some(self)
    }

    fn remote_encoder(&self) -> Option<&dyn RemoteEncoder> {
        Some(self)
    }
}

impl<T: Transport> Supervised for RemoteSource<T> {
    fn observe(&mut self, telemetry: Option<EncoderTelemetry>) {
        self.telemetry = telemetry;
    }

    fn health(&self) -> CaptureHealth {
        self.supervisor
            .report((self.clocks.now)(), self.telemetry.as_ref())
    }

    fn stage_done(&mut self, stage: Stage, outcome: StageOutcome) {
        self.stage_done_with(stage, outcome, &mut NoRungs);
    }
}

impl<T: Transport> RemoteEncoder for RemoteSource<T> {
    fn site(&self) -> EncoderSite {
        self.transport.endpoint().site
    }

    fn open(&self, params: &DriverEncodeParams) -> Result<Box<dyn pf_encode_core::Encoder>> {
        encoder::open(&self.transport, params, self.clocks, &self.poke)
    }

    fn arrival_poke(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let poke = self.poke.clone();
        Some(Arc::new(move || poke.fire()))
    }
}
