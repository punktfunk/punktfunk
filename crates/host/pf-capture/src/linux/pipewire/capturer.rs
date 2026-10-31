//! The PipeWire capturer's front end: [`PortalCapturer`] for the xdg ScreenCast portal
//! (`ashpd`) and for a virtual output's node, plus its first-frame timeout verdicts.
//!
//! Two dedicated threads because both stacks are thread-tied:
//! - **portal** — async ashpd handshake on a multi-thread tokio runtime
//!   (control plane, never per-frame), then parks on a oneshot so the
//!   `proxy` and its zbus connection stay alive. Ashpd's `Session` has
//!   no `Drop`; the compositor tears the cast down when that connection
//!   drops.
//! - **pipewire** — owns the `!Send` MainLoop/Stream and pumps frames.
//!
//! [`PortalCapturer`]'s `Drop` quits and joins the pipewire thread; [`PortalSession`]'s
//! `Drop` fires the portal oneshot and waits bounded so the zbus drop ends the ScreenCast.

use super::config::StreamConfig;
use super::consume::FenceWaitStats;
use super::plan::{
    gpu_import, negotiation_plan, ImportOutcome, ImportPolicy, ImportState, NegotiationInputs,
};
use super::queue::{wait_ready, FrameQueue};
use super::thread::{pipewire_thread, LoopEnds, PwCtl};
use super::CaptureOpts;
use crate::knobs::Knobs;
use crate::linux::portal::portal_thread;
use crate::linux::{
    drain_edges, health_identity, xfixes_cursor, CursorSlot, ImporterCell, Negotiation,
    StreamShared, StreamStatus,
};
use crate::{CapturedFrame, Capturer, FramePayload, ZeroCopyPolicy};
use anyhow::{anyhow, Context, Result};
use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Portal + PipeWire capturer, reused across streams. [`set_active`] gates
/// the per-frame de-pad so the screencast stays up between reconnects.
pub struct PortalCapturer {
    /// Arrivals awaiting their renders. The wait on a render runs on this side, never on
    /// the PipeWire thread: that one hands the producer's buffers back.
    queue: FrameQueue,
    /// Wakeup only — the queue holds the frames, so a coalesced edge loses
    /// nothing. Sender dies with the PipeWire thread (`Disconnected`).
    wake: Receiver<()>,
    /// Set by [`crate::Feed::arrival_poke`]; `wait_ready` takes it and returns with no frame.
    poked: Arc<std::sync::atomic::AtomicBool>,
    /// Arrival to take, per frame (`PUNKTFUNK_PERF`).
    fence_wait: FenceWaitStats,
    shared: StreamShared,
    cursor_slot: CursorSlot,
    importer: ImporterCell,
    /// `shared.resent` as of the last [`Capturer::events`].
    resent_acked: u64,
    /// First drop out of `Streaming` with no frame. Grace for a transient
    /// renegotiation; cleared on a frame or when `Streaming` again.
    stall_since: Option<std::time::Instant>,
    /// Raw-dmabuf passthrough offer, copied from the thread's
    /// [`NegotiationPlan`](super::plan::NegotiationPlan) — never re-derived.
    /// A failed offer latches this capture's [`pf_zerocopy::ZeroCopyHealth`].
    vaapi_dmabuf: bool,
    /// CUDA import choices for held frames ([`Self::import_held`]).
    import_policy: ImportPolicy,
    /// Held frames pass through as dmabufs: the NVENC encoder's worker converts them. A
    /// failed offer latches the same raw-dmabuf latch the VAAPI passthrough uses.
    raw_for_encoder: bool,
    /// This consumer's import memory (LINEAR NV12 latch, tiled failure streak).
    import_state: ImportState,
    /// One-shot: this capture's dmabuf offer negotiated; retry budget credited.
    negotiation_confirmed: bool,
    /// HDR offer. A failed negotiation latches SDR for [`Self::hdr_source`]
    /// only, not process-wide.
    hdr_offer: bool,
    /// Latch target for a failed [`hdr_offer`](Self::hdr_offer). See [`crate::HdrSource`].
    hdr_source: crate::HdrSource,
    node_id: u32,
    /// `Drop` sends this. Without it `mainloop.run()` blocks until process
    /// exit (leaks the thread and EGL/CUDA). `Option` so `Drop` can take it.
    quit: Option<::pipewire::channel::Sender<PwCtl>>,
    /// Joined in `Drop` after `quit` so the importer/CUDA is gone before
    /// the next pipeline builds.
    join: Option<thread::JoinHandle<()>>,
    /// Virtual output; its `Drop` releases the compositor output. `None` on
    /// the portal path (the portal thread closes its session), or once a
    /// capture-only rebuild took it back (`take_keepalive`).
    keepalive: Option<Box<dyn Send>>,
    /// Portal-thread teardown. `None` on the virtual-output path. Its `Drop`
    /// ends the compositor's screencast.
    _portal: Option<PortalSession>,
    /// Gamescope XFixes reader; `Drop` stops the thread. `None` on the portal
    /// path (`SPA_META_Cursor`).
    _gs_cursor: Option<xfixes_cursor::XFixesCursorSource>,
}

/// Portal-thread teardown. Firing `quit` un-parks the thread, which closes
/// the portal session. Ashpd's `Session` has no `Drop` and the zbus connection
/// is process-global, so `Session.Close` is what ends the compositor's cast.
struct PortalSession {
    /// `Option` so `Drop` can take it. Dropping the sender without a send
    /// resolves the receiver with `Err`; the thread treats both the same.
    quit: Option<tokio::sync::oneshot::Sender<()>>,
    /// Fired after the session is closed, so `Drop` can bound its wait instead
    /// of `join()` behind a wedged D-Bus round-trip.
    done: Receiver<()>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        // Bounded wait: the thread may be in a D-Bus round-trip against a
        // wedged portal; an unbounded `join()` hangs the host. On timeout
        // detach — the thread finishes its Close on its own.
        drop(self.quit.take()); // send-or-drop: both resolve the receiver
        let joinable = match self.done.recv_timeout(Duration::from_millis(750)) {
            Ok(()) => true,
            Err(_) => {
                tracing::warn!(
                    "portal thread did not close its session within 750ms — detaching it (the \
                     compositor's cast lingers until the Close lands or the host exits)"
                );
                false
            }
        };
        if let Some(join) = self.join.take() {
            if joinable {
                let _ = join.join();
            }
        }
    }
}

impl PortalCapturer {
    /// `anchored` drives ScreenCast off a RemoteDesktop session so it
    /// inherits that grant (no second dialog). `false` is a plain ScreenCast
    /// (wlroots has no RemoteDesktop portal). `want_metadata_cursor` asks
    /// for `SPA_META_Cursor` vs compositor-embedded pointer
    /// ([`pf_portal::negotiate_cursor_mode`]). Its YUV is at `full_range` or studio.
    pub fn open(
        anchored: bool,
        want_hdr: bool,
        want_metadata_cursor: bool,
        full_range: bool,
        policy: ZeroCopyPolicy,
        knobs: Knobs,
    ) -> Result<PortalCapturer> {
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(OwnedFd, u32), String>>();
        let (quit_tx, quit_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = sync_channel::<()>(1);
        let join = thread::Builder::new()
            .name("punktfunk-portal".into())
            .spawn(move || {
                portal_thread(setup_tx, quit_rx, want_metadata_cursor, anchored);
                // After the fn closed its portal session, so `Drop`'s
                // `recv_timeout` means the cast is gone. Covers early returns.
                let _ = done_tx.send(());
            })
            .context("spawn portal thread")?;
        let portal = PortalSession {
            quit: Some(quit_tx),
            done: done_rx,
            join: Some(join),
        };

        let (fd, node_id) = match setup_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(anyhow!("ScreenCast portal setup failed: {e}")),
            Err(_) => return Err(anyhow!("timed out waiting for the ScreenCast portal")),
        };
        tracing::info!(
            node_id,
            want_hdr,
            "ScreenCast portal session started; connecting PipeWire"
        );
        // Monitor capture is 4:2:0, so zero-copy is allowed. The `?` drops
        // `portal` on spawn failure and tears the screencast down.
        Ok(spawn_pipewire(
            Some(fd),
            node_id,
            None,
            CaptureOpts {
                allow_zerocopy: true,
                want_444: false,
                want_hdr,
                ten_bit_sdr: false,
                sdr10_native: false,
                full_range,
                expect_exact_dims: false,
                // Portal-monitor is Mutter's stale-meta id-0 contract. KWin
                // portal capture would rewrite per buffer; nothing routes
                // one here yet (`from_virtual_output` carries the real flag).
                cursor_id0_hides: false,
                producer_is_gamescope: false,
                pool_min: crate::POOL_MIN,
                pool_max: None,
                unpaced: false,
                // A monitor mirror paints on the panel's own vblank; nothing to drive.
                lazy: false,
                stream_hz: 0,
                knobs,
            },
            policy,
        )?
        .into_capturer(node_id, None, Some(portal), crate::HdrSource::PortalMonitor))
    }

    /// Capturer for an already-created virtual output's PipeWire node.
    /// `opts.producer` supplies the producer contracts a node id cannot reveal, and
    /// `opts.studio_range` the session's range. `keepalive` releases the output with the
    /// capturer.
    pub fn from_virtual_output(
        remote_fd: Option<OwnedFd>,
        node_id: u32,
        preferred_mode: Option<(u32, u32, u32)>,
        keepalive: Box<dyn Send>,
        opts: crate::VirtualOutputOpts,
        knobs: Knobs,
    ) -> Result<PortalCapturer> {
        let kwin = opts.producer == crate::Producer::Kwin;
        let capture = CaptureOpts {
            allow_zerocopy: opts.allow_zerocopy,
            want_444: opts.want_444,
            want_hdr: opts.want_hdr,
            ten_bit_sdr: opts.ten_bit_sdr,
            sdr10_native: opts.sdr10_native,
            full_range: !opts.studio_range,
            expect_exact_dims: opts.expect_exact_dims,
            cursor_id0_hides: kwin,
            producer_is_gamescope: opts.producer == crate::Producer::Gamescope,
            pool_min: if kwin {
                crate::KWIN_POOL_MIN
            } else {
                crate::POOL_MIN
            },
            pool_max: kwin.then_some(crate::KWIN_POOL_MAX),
            unpaced: kwin && !knobs.kwin_paced,
            lazy: knobs.lazy_capture,
            stream_hz: opts.stream_hz,
            knobs,
        };
        tracing::info!(
            node_id,
            allow_zerocopy = capture.allow_zerocopy,
            want_444 = capture.want_444,
            want_hdr = capture.want_hdr,
            full_range = capture.full_range,
            expect_exact_dims = capture.expect_exact_dims,
            producer = ?opts.producer,
            pool_min = capture.pool_min,
            pool_max = ?capture.pool_max,
            unpaced = capture.unpaced,
            "connecting PipeWire to virtual output"
        );
        // Virtual outputs are SDR-only except a gamescope node from our
        // `pipewire-hdr` build — the host checks before Welcome
        // (`capture::capturer_supports_hdr_for`).
        let mut capturer =
            spawn_pipewire(remote_fd, node_id, preferred_mode, capture, opts.policy)?
                .into_capturer(
                    node_id,
                    Some(keepalive),
                    None,
                    crate::HdrSource::VirtualOutput,
                );
        capturer._gs_cursor = opts.gamescope_cursor.and_then(|targets| {
            xfixes_cursor::XFixesCursorSource::spawn(
                targets,
                capturer.cursor_slot.clone(),
                Arc::clone(&capturer.shared.negotiation),
            )
        });
        Ok(capturer)
    }
}

struct PwHandles {
    queue: FrameQueue,
    wake: Receiver<()>,
    shared: StreamShared,
    cursor_slot: CursorSlot,
    importer: ImporterCell,
    vaapi_dmabuf: bool,
    hdr_offer: bool,
    /// Copied from the plan: what the consumer's import of a held frame does.
    import_policy: ImportPolicy,
    /// Held frames pass through as dmabufs: the NVENC encoder converts them itself.
    raw_for_encoder: bool,
    quit: ::pipewire::channel::Sender<PwCtl>,
    join: thread::JoinHandle<()>,
}

impl PwHandles {
    /// `keepalive` owns the virtual output and drops after the PipeWire
    /// thread is joined. `portal` is teardown for [`PortalCapturer::open`].
    fn into_capturer(
        self,
        node_id: u32,
        keepalive: Option<Box<dyn Send>>,
        portal: Option<PortalSession>,
        hdr_source: crate::HdrSource,
    ) -> PortalCapturer {
        PortalCapturer {
            queue: self.queue,
            wake: self.wake,
            poked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fence_wait: Default::default(),
            shared: self.shared,
            cursor_slot: self.cursor_slot,
            importer: self.importer,
            resent_acked: 0,
            stall_since: None,
            vaapi_dmabuf: self.vaapi_dmabuf,
            import_policy: self.import_policy,
            raw_for_encoder: self.raw_for_encoder,
            import_state: ImportState::default(),
            negotiation_confirmed: false,
            hdr_offer: self.hdr_offer,
            hdr_source,
            node_id,
            quit: Some(self.quit),
            join: Some(self.join),
            keepalive,
            _portal: portal,
            _gs_cursor: None,
        }
    }
}

/// Spawn one PipeWire consumer with health keyed by source and encoder policy. Its
/// [`StreamConfig`] resolves here, once: every env and latch read the plan rests on.
/// `preferred` seeds negotiation and sizes a Mutter virtual monitor.
fn spawn_pipewire(
    fd: Option<OwnedFd>,
    node_id: u32,
    preferred: Option<(u32, u32, u32)>,
    opts: CaptureOpts,
    // Encode-backend facts from the facade; never re-derived here.
    policy: ZeroCopyPolicy,
) -> Result<PwHandles> {
    // `expect_exact_dims` is forwarded to the thread inside `opts`, not read here.
    let CaptureOpts {
        allow_zerocopy,
        want_444,
        want_hdr,
        sdr10_native,
        ..
    } = opts;
    // Wakeup edges only; depth 1 is right — a coalesced edge loses nothing
    // because the queue holds the frames.
    let queue = FrameQueue::default();
    let (wake_tx, wake_rx) = sync_channel::<()>(1);
    // Portal-fd vs virtual-output with the same node number are different sources.
    let identity = u64::from(node_id) | (u64::from(fd.is_some()) << 32);
    let shared = StreamShared::new(pf_zerocopy::zero_copy_health(health_identity(
        identity, &policy,
    )));
    let (cursor_slot, importer) = (CursorSlot::default(), ImporterCell::default());
    let ends = LoopEnds {
        queue: queue.clone(),
        wake: wake_tx,
        shared: shared.clone(),
        cursor_slot: cursor_slot.clone(),
        importer: importer.clone(),
    };
    // Receiver attaches to the loop; sender fires in `Drop`.
    let (quit_tx, quit_rx) = ::pipewire::channel::channel::<PwCtl>();
    let zerocopy = allow_zerocopy && pf_zerocopy::enabled();
    // HDR cannot ride SHM: FORCE_SHM drops the HDR offer (SDR, loudly).
    let force_shm = opts.knobs.force_shm;
    let want_hdr = if want_hdr && force_shm {
        tracing::warn!(
            "HDR capture requested but PUNKTFUNK_FORCE_SHM=1 — the SHM path is 8-bit only; \
             offering SDR"
        );
        false
    } else {
        want_hdr
    };
    let plan = negotiation_plan(NegotiationInputs {
        zerocopy,
        force_shm,
        want_hdr,
        // SHM is 8-bit, like HDR above.
        sdr10_native: sdr10_native && !force_shm,
        want_444,
        backend_is_vaapi: policy.backend_is_vaapi,
        pyrowave_session: policy.pyrowave_session,
        native_nv12_session: policy.native_nv12_session,
        ladder: shared.health.ladder(),
        native_nv12_knob: opts.knobs.pipewire_nv12,
        hdr_cuda_ok: policy.hdr_cuda_ok,
        nv12_knob: pf_zerocopy::nv12_enabled(),
        nvenc_raw: crate::linux::raw_lane_ok(&policy),
    });
    let vaapi_dmabuf = plan.vaapi_passthrough;
    let import_policy = plan.import_policy;
    let raw_for_encoder = plan.nvenc_raw;
    let cfg = StreamConfig {
        node_id,
        // `allow_zerocopy` is already in `plan`; `want_hdr` may have been cleared by FORCE_SHM.
        opts: CaptureOpts { want_hdr, ..opts },
        preferred,
        policy,
        plan,
    };
    let join = thread::Builder::new()
        .name("punktfunk-pipewire".into())
        .spawn(move || {
            if let Err(e) = pipewire_thread(cfg, fd, ends, quit_rx) {
                tracing::error!(error = %format!("{e:#}"), "pipewire capture thread failed");
            }
        })
        .context("spawn pipewire thread")?;
    Ok(PwHandles {
        queue,
        wake: wake_rx,
        shared,
        cursor_slot,
        importer,
        vaapi_dmabuf,
        hdr_offer: want_hdr,
        import_policy,
        raw_for_encoder,
        quit: quit_tx,
        join,
    })
}

/// A full first-frame wait: long enough for a cold gamescope's first frame. A compositor that
/// never fixates a format sends no other signal, so only this expiry convicts an offer.
const FIRST_FRAME_BUDGET: Duration = Duration::from_secs(10);

impl Capturer for PortalCapturer {
    fn next_frame(&mut self, budget: crate::FirstFrame) -> Result<CapturedFrame> {
        use crate::FirstFrame;
        match budget {
            FirstFrame::Default => {
                self.frame_within(FIRST_FRAME_BUDGET, TimeoutVerdict::Conclusive)
            }
            FirstFrame::Within(b) => self.frame_within(b, TimeoutVerdict::Conclusive),
            // Truncated first attempt: expiry re-runs the schedule, does not convict an offer.
            FirstFrame::Provisional(b) => self.frame_within(b, TimeoutVerdict::Provisional),
        }
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        if self.shared.status() == StreamStatus::Broken {
            return Err(anyhow!(
                "zero-copy GPU import lost (node {}): the import worker died, or a tiled or \
                 planar offer failed — rebuilding capture",
                self.node_id
            )
            .context(crate::DisplayStillAlive));
        }
        // A dead thread's leftover frame is still served first.
        let producer_gone = drain_edges(&self.wake);
        let latest = self.take_frame();
        if producer_gone && latest.is_none() {
            return Err(anyhow!("PipeWire capture thread ended"));
        }
        if latest.is_some() || self.shared.status().is_streaming() {
            self.stall_since = None;
            return Ok(latest);
        }
        // Left `Streaming` with no frame. Grace a renegotiation blip before
        // declaring the source lost (else freeze on the last frame).
        const STALL_GRACE: Duration = Duration::from_millis(1500);
        let since = *self.stall_since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() >= STALL_GRACE {
            self.stall_since = None;
            return Err(anyhow!(
                "PipeWire source stalled (node {}): stream left Streaming for >{}ms with no frames \
                 — the compositor/virtual output went away (session switch?)",
                self.node_id,
                STALL_GRACE.as_millis()
            ));
        }
        Ok(latest)
    }

    /// `Streaming` on a running loop thread, no frame consumed. A thread that exited keeps
    /// its last status, so the join handle decides. A static desktop stays `Streaming` (no
    /// buffers) and is not reported dead.
    fn is_alive(&self) -> bool {
        self.shared.status().is_streaming() && self.join.as_ref().is_some_and(|j| !j.is_finished())
    }

    /// A re-sent buffer since the last call: the frame encoded from the earlier read may be
    /// torn, so the loop refreshes the reference with one IDR.
    fn events(&mut self) -> Vec<crate::SourceEvent> {
        let n = self.shared.resent.load(Ordering::Relaxed);
        let risk = n != self.resent_acked;
        self.resent_acked = n;
        if risk {
            vec![crate::SourceEvent::ReferenceRisk]
        } else {
            Vec::new()
        }
    }

    /// [`pf_frame::hdr::generic_hdr10`] once 10-bit PQ is fixated. Neither Linux producer
    /// exposes mastering through the screencast (Mutter has none; gamescope's
    /// `VK_EXT_hdr_metadata` stops at the compositor). The native loop prefers the client's
    /// volume when sent (`Hello::display_hdr`).
    fn colour(&self) -> crate::ColourFacts {
        let hdr = self.shared.negotiation().fixated.is_some_and(|f| f.hdr);
        crate::ColourFacts {
            hdr_meta: hdr.then(pf_frame::hdr::generic_hdr10),
            sdr_fp16: false,
        }
    }

    fn feed(&mut self) -> Option<&mut dyn crate::Feed> {
        Some(self)
    }

    fn cursor(&mut self) -> Option<&mut dyn crate::CursorSource> {
        Some(self)
    }
}

impl crate::Feed for PortalCapturer {
    fn wait_arrival(&mut self, deadline: std::time::Instant) {
        // A driven producer paints on its own requests (`pacer::Pacer`), so
        // this wait never triggers a paint. It returns on a finished render, not on an
        // arrival: the producer sends a buffer before it has painted it.
        if self.shared.status() != StreamStatus::Broken {
            let _ = wait_ready(&self.queue, &self.wake, &self.poked, deadline);
        }
    }

    fn arrival_poke(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let (poked, ctl) = (self.poked.clone(), self.quit.clone()?);
        Some(Arc::new(move || {
            poked.store(true, Ordering::Relaxed);
            let _ = ctl.send(PwCtl::Poke);
        }))
    }

    fn set_active(&mut self, active: bool) {
        self.shared.active.store(active, Ordering::Relaxed);
        if !active {
            // Flush: a reused capturer would hand the next stream the previous
            // session's last frame (`pts_ns` from the old clock). Producer
            // stops publishing while inactive.
            if let Ok(mut queue) = self.queue.lock() {
                queue.clear();
            }
            // Else a leftover `Instant` expires the 1500 ms grace on the first
            // `try_latest` of a stream that has been running for microseconds.
            self.stall_since = None;
        }
    }

    /// Only the virtual-output path holds one; the portal thread owns its own session. The
    /// taker links a new stream to the output, so this one stops first: buffers an old link
    /// still holds when a second one joins stay lost to the producer (KWin: 120 → 30 fps).
    fn take_keepalive(&mut self) -> Option<Box<dyn Send>> {
        let keepalive = self.keepalive.take();
        if keepalive.is_some() {
            self.stop_stream();
        }
        keepalive
    }

    fn pool_use(&self) -> Option<crate::PoolUse> {
        let gauge = &self.shared.pool;
        Some(crate::PoolUse {
            depth: gauge.depth.load(Ordering::Relaxed),
            withheld_peak: gauge.withheld_peak.swap(0, Ordering::Relaxed),
        })
    }
}

impl crate::CursorSource for PortalCapturer {
    fn overlay(&mut self) -> Option<pf_frame::CursorOverlay> {
        // Includes cursor-only buffers. Gamescope fills this via XFixes.
        self.cursor_slot
            .live
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
    }

    fn set_forward(&mut self, _client_draws: bool) {
        // Either way the host places the pointer from `overlay()`: forwarded, or on the frame
        // for the encoder blend.
        self.cursor_slot
            .host_places
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Whether an expired first-frame budget may convict an offer. The retry
/// loop's truncated first attempt is `Provisional`: expiry means the
/// schedule moved on, not that the compositor refused. Only a full-length
/// wait may latch — a gamescope cold start needs that extra window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TimeoutVerdict {
    Conclusive,
    Provisional,
}

/// Offer a first-frame timeout implicates. Split out so the latch policy
/// is testable. A negotiated format clears every offer; forced
/// `PUNKTFUNK_ZEROCOPY=1` keeps both dmabuf arms erroring (operator asked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TimeoutOffer {
    /// Format negotiated; compositor produced no buffers.
    NoBuffers,
    Hdr,
    RawDmabuf,
    GpuDmabuf,
    /// Nothing negotiated — format/modifier mismatch.
    NoFormat,
}

fn classify_first_frame_timeout(
    negotiation: Negotiation,
    hdr_offer: bool,
    vaapi_dmabuf: bool,
    zerocopy_forced: bool,
) -> TimeoutOffer {
    if negotiation.fixated.is_some() {
        TimeoutOffer::NoBuffers
    } else if hdr_offer {
        TimeoutOffer::Hdr
    } else if vaapi_dmabuf && !zerocopy_forced {
        TimeoutOffer::RawDmabuf
    } else if negotiation.gpu_offer && !zerocopy_forced {
        TimeoutOffer::GpuDmabuf
    } else {
        TimeoutOffer::NoFormat
    }
}

fn timeout_convicts(offer: TimeoutOffer, verdict: TimeoutVerdict) -> bool {
    verdict == TimeoutVerdict::Conclusive
        && matches!(
            offer,
            TimeoutOffer::Hdr | TimeoutOffer::RawDmabuf | TimeoutOffer::GpuDmabuf
        )
}

/// The zero-copy lane an expiry costs a strike. HDR has its own latch, per `HdrSource`.
pub(super) fn convicted_lane(
    offer: TimeoutOffer,
    verdict: TimeoutVerdict,
) -> Option<pf_zerocopy::Lane> {
    let lane = match offer {
        TimeoutOffer::RawDmabuf => pf_zerocopy::Lane::Raw,
        TimeoutOffer::GpuDmabuf => pf_zerocopy::Lane::Gpu,
        _ => return None,
    };
    timeout_convicts(offer, verdict).then_some(lane)
}

impl PortalCapturer {
    /// First frame can lag negotiation; later frames arrive at ~fps. Wait in
    /// 500 ms slices so a GPU-import poison or an errored stream fails within
    /// ~0.5 s instead of the full first-frame budget.
    fn frame_within(&mut self, budget: Duration, verdict: TimeoutVerdict) -> Result<CapturedFrame> {
        let started = std::time::Instant::now();
        let deadline = started + budget;
        loop {
            if self.shared.status() == StreamStatus::Broken {
                return Err(anyhow!(
                    "zero-copy GPU import lost (node {}): the import worker died, or a tiled or \
                     planar offer failed — rebuilding capture",
                    self.node_id
                ));
            }
            // Queue before wakeup: a coalesced edge (or a publish while we
            // were not waiting) is still visible.
            if let Some(f) = self.take_frame() {
                self.note_negotiation_confirmed();
                return Ok(f);
            }
            // Judged like an expired wait, over the time actually spent.
            if self.shared.status() == StreamStatus::Errored {
                let spent = started.elapsed();
                return self.next_frame_timed_out(RecvTimeoutError::Timeout, spent, verdict);
            }
            let slice = Duration::from_millis(500)
                .min(deadline.saturating_duration_since(std::time::Instant::now()));
            let until = std::time::Instant::now() + slice;
            let end = if !wait_ready(&self.queue, &self.wake, &self.poked, until) {
                RecvTimeoutError::Disconnected
            } else if std::time::Instant::now() >= deadline {
                RecvTimeoutError::Timeout
            } else {
                continue;
            };
            // A last frame can sit in the queue even as the producer exits.
            if let Some(f) = self.take_frame() {
                return Ok(f);
            }
            return self.next_frame_timed_out(end, budget, verdict);
        }
    }

    /// The next frame whose render finished; a frame it passes over goes back to the producer.
    fn take_frame(&mut self) -> Option<CapturedFrame> {
        let taken = self
            .queue
            .lock()
            .ok()?
            .take_ready(std::time::Instant::now())?;
        self.fence_wait.took(&taken);
        let frame = taken.frame;
        if self.vaapi_dmabuf
            || self.raw_for_encoder
            || !matches!(frame.payload, FramePayload::Dmabuf(_))
        {
            return Some(frame);
        }
        self.import_held(frame)
    }

    /// CUDA-import a held dmabuf at the consumer's tick. The hold, and with it
    /// the producer's buffer, returns when `frame` drops here, import or not. A
    /// LINEAR failure retires the importer: later arrivals take the CPU path.
    fn import_held(&mut self, frame: CapturedFrame) -> Option<CapturedFrame> {
        let CapturedFrame {
            width,
            height,
            pts_ns,
            format,
            payload,
            cursor,
            provenance,
        } = frame;
        let FramePayload::Dmabuf(held) = payload else {
            return None;
        };
        let cell = self.importer.slot.clone();
        let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
        let importer = guard.as_mut()?;
        let plane = pf_zerocopy::DmabufPlane {
            fd: std::os::fd::AsRawFd::as_raw_fd(&held.fd),
            offset: held.offset,
            stride: held.stride,
        };
        match gpu_import(
            importer,
            self.import_policy,
            &mut self.import_state,
            &self.shared,
            format,
            width,
            height,
            plane,
            held.modifier,
        ) {
            ImportOutcome::Frame(buf, format) => Some(CapturedFrame {
                width,
                height,
                pts_ns,
                format,
                payload: FramePayload::Cuda(buf),
                cursor,
                provenance,
            }),
            ImportOutcome::Dropped => None,
            ImportOutcome::ImporterLost => {
                *guard = None;
                self.importer.present.store(false, Ordering::Relaxed);
                None
            }
        }
    }

    /// Credit the dmabuf negotiation retry budget. Once per capture: the
    /// budget counts consecutive failed builds, not frames.
    fn note_negotiation_confirmed(&mut self) {
        if (self.vaapi_dmabuf || self.raw_for_encoder) && !self.negotiation_confirmed {
            self.negotiation_confirmed = true;
            let taken = pf_zerocopy::Evidence::OfferTaken(pf_zerocopy::Lane::Raw);
            self.shared.health.note(&[taken]);
        }
    }

    /// Budget expired or the thread ended. A lane takes a strike only when the
    /// expiry convicts its offer ([`convicted_lane`]).
    fn next_frame_timed_out(
        &self,
        err: RecvTimeoutError,
        budget: Duration,
        verdict: TimeoutVerdict,
    ) -> Result<CapturedFrame> {
        let within = budget.as_secs_f32();
        match err {
            RecvTimeoutError::Timeout => {
                let offer = classify_first_frame_timeout(
                    self.shared.negotiation(),
                    self.hdr_offer,
                    self.vaapi_dmabuf,
                    pf_zerocopy::zerocopy_forced(),
                );
                let convicted = timeout_convicts(offer, verdict);
                if let Some(lane) = convicted_lane(offer, verdict) {
                    self.shared.health.note_offer_timed_out(lane);
                }
                // Provisional names the suspect but does not latch; the
                // full-length retry's timeout does.
                let sentence = if convicted {
                    "" // each arm below states its own downgrade
                } else {
                    " (short first-attempt window — nothing is latched; the full-length retry \
                     decides)"
                };
                match offer {
                    TimeoutOffer::NoBuffers => Err(anyhow!(
                        "no PipeWire frame within {within}s (node {}): format negotiated but no \
                         buffers arrived — the compositor produced no frames (virtual output \
                         idle/unmapped, capture never started, or a stream bound during a \
                         compositor (re)start that will never deliver — a reconnect fixes that)",
                        self.node_id
                    )),
                    TimeoutOffer::Hdr => {
                        // Latch SDR for this `HdrSource` only — a process-wide
                        // flag let either Linux HDR source disable the other.
                        if convicted {
                            crate::note_hdr_capture_failed(self.hdr_source);
                        }
                        Err(anyhow!(
                            "no PipeWire frame within {within}s (node {}): the compositor never \
                             accepted the HDR (10-bit PQ/BT.2020 dmabuf) offer — is the mirrored \
                             monitor in HDR mode on GNOME 50+?{}",
                            self.node_id,
                            if convicted {
                                " Downgrading this host to SDR capture; reconnect to stream SDR"
                            } else {
                                sentence
                            }
                        ))
                    }
                    TimeoutOffer::RawDmabuf => Err(anyhow!(
                        "no PipeWire frame within {within}s (node {}): the compositor never \
                         accepted the dmabuf-only offer (raw-dmabuf passthrough){}",
                        self.node_id,
                        if convicted {
                            " — downgrading THIS path to CPU capture for the rest of the \
                             process; the pipeline rebuild will renegotiate without dmabuf"
                        } else {
                            sentence
                        }
                    )),
                    TimeoutOffer::GpuDmabuf => Err(anyhow!(
                        "no PipeWire frame within {within}s (node {}): the compositor never \
                         accepted the dmabuf-only offer (EGL→CUDA GPU import){}",
                        self.node_id,
                        if convicted {
                            " — downgrading THIS offer to the CPU path for the rest of the \
                             process; the pipeline rebuild will renegotiate without dmabuf"
                        } else {
                            sentence
                        }
                    )),
                    TimeoutOffer::NoFormat => Err(anyhow!(
                        "no PipeWire frame within {within}s (node {}): format negotiation never \
                         completed — the compositor offered no format this consumer accepts \
                         (pixel-format/modifier mismatch) or the node never emitted a Format param",
                        self.node_id
                    )),
                }
            }
            RecvTimeoutError::Disconnected => Err(anyhow!(
                "PipeWire capture thread ended before a frame (node {})",
                self.node_id
            )),
        }
    }
}

impl PortalCapturer {
    /// Quit the PipeWire thread and join it: the stream disconnects and EGL/CUDA are released.
    /// Without this `mainloop.run()` blocks until process exit. `send` errs only if the thread
    /// already exited.
    fn stop_stream(&mut self) {
        if let Some(quit) = self.quit.take() {
            let _ = quit.send(PwCtl::Quit);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for PortalCapturer {
    fn drop(&mut self) {
        // Before keepalive drops: the stream lets go of the output before the output goes.
        self.stop_stream();
    }
}

#[cfg(test)]
mod first_frame_timeout_tests {
    use super::{classify_first_frame_timeout, timeout_convicts, TimeoutOffer, TimeoutVerdict};
    use crate::linux::{Fixated, Negotiation};

    #[test]
    fn a_provisional_expiry_convicts_no_offer_whatever_was_on_the_table() {
        // Truncated first attempt must not latch. A gamescope HDR cold start
        // needs longer than that window; a latch would pin SDR + CPU for
        // the process lifetime.
        for offer in [
            TimeoutOffer::NoBuffers,
            TimeoutOffer::Hdr,
            TimeoutOffer::RawDmabuf,
            TimeoutOffer::GpuDmabuf,
            TimeoutOffer::NoFormat,
        ] {
            assert!(
                !timeout_convicts(offer, TimeoutVerdict::Provisional),
                "provisional expiry must not latch {offer:?}"
            );
        }
    }

    #[test]
    fn a_conclusive_expiry_convicts_exactly_the_offer_bearing_diagnoses() {
        assert!(timeout_convicts(
            TimeoutOffer::Hdr,
            TimeoutVerdict::Conclusive
        ));
        assert!(timeout_convicts(
            TimeoutOffer::RawDmabuf,
            TimeoutVerdict::Conclusive
        ));
        assert!(timeout_convicts(
            TimeoutOffer::GpuDmabuf,
            TimeoutVerdict::Conclusive
        ));
        // Negotiated-but-idle and format mismatch implicate no offer.
        assert!(!timeout_convicts(
            TimeoutOffer::NoBuffers,
            TimeoutVerdict::Conclusive
        ));
        assert!(!timeout_convicts(
            TimeoutOffer::NoFormat,
            TimeoutVerdict::Conclusive
        ));
    }

    /// Each verdict outranks every one after it: with its own cause and every later cause
    /// present, it is the one named. HDR outranks the dmabuf arms — it is the offer that failed.
    #[test]
    fn classification_mirrors_the_negotiation_state_precedence() {
        use TimeoutOffer::*;
        let order = [NoBuffers, Hdr, RawDmabuf, GpuDmabuf, NoFormat];
        let fixated = Fixated {
            hdr: false,
            size: (1, 1),
        };
        for (rank, &want) in order.iter().enumerate() {
            let cause = |offer| order[rank..].contains(&offer);
            let negotiation = Negotiation {
                gpu_offer: cause(GpuDmabuf),
                fixated: cause(NoBuffers).then_some(fixated),
            };
            let got =
                classify_first_frame_timeout(negotiation, cause(Hdr), cause(RawDmabuf), false);
            assert_eq!(got, want);
        }
    }

    #[test]
    fn a_forced_zerocopy_keeps_both_dmabuf_arms_erroring_loudly_instead_of_implicated() {
        // `PUNKTFUNK_ZEROCOPY=1` is the operator insisting on the path —
        // timeout falls through to the generic diagnosis and never latches.
        let offered = Negotiation {
            gpu_offer: true,
            fixated: None,
        };
        assert_eq!(
            classify_first_frame_timeout(offered, false, true, true),
            TimeoutOffer::NoFormat
        );
    }
}

/// What each consumer-side reader answers in each state the loop thread's writes reach.
#[cfg(test)]
mod status_reader_tests {
    use super::{PortalCapturer, TimeoutVerdict};
    use crate::linux::{StreamShared, StreamStatus};
    use crate::{CapturedFrame, Capturer, Feed, FramePayload, PixelFormat};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::sync::mpsc::{channel, sync_channel, Sender, SyncSender};
    use std::time::{Duration, Instant};

    /// A state the loop thread leaves the stream in.
    #[derive(Clone, Copy, Debug)]
    enum At {
        Fresh,
        Fixated,
        Streaming,
        Driving,
        /// Left `Streaming` after the format, as a renegotiation does.
        Paused,
        /// `Error` before any format, as "no more input formats" is.
        ErroredUnfixated,
        Errored,
        /// GPU import lost while streaming.
        Broken,
        BrokenErrored,
        /// `Streaming` over a Format pod that did not parse.
        StreamingUnfixated,
    }

    /// Leave `s` as the loop thread's writes leave it at `at`.
    fn settle(s: &StreamShared, at: At, hdr: bool) {
        use At::*;
        let idle = StreamStatus::Streaming { driving: false };
        if !matches!(at, Fresh | ErroredUnfixated | StreamingUnfixated) {
            s.fixate(crate::linux::Fixated { hdr, size: (1, 1) });
        }
        let reports: &[StreamStatus] = match at {
            Fresh | Fixated => &[],
            Streaming | Broken | StreamingUnfixated => &[idle],
            Driving => &[StreamStatus::Streaming { driving: true }],
            Paused => &[idle, StreamStatus::Paused],
            ErroredUnfixated | Errored | BrokenErrored => &[StreamStatus::Errored],
        };
        for &r in reports {
            s.report(r);
        }
        if matches!(at, Broken | BrokenErrored) {
            s.mark_broken();
        }
    }

    /// A capturer at `at` with an empty queue. Fields drop in order: the parked loop thread
    /// ends before the capturer's `Drop` joins it.
    struct Rig {
        parked: Option<Sender<()>>,
        edge: Option<SyncSender<()>>,
        cap: PortalCapturer,
    }

    fn rig(at: At, hdr: bool) -> Rig {
        static IDENTITY: AtomicU64 = AtomicU64::new(0x5747_0000_0000);
        let (edge, wake) = sync_channel(1);
        let (parked, park) = channel::<()>();
        let join = std::thread::spawn(move || {
            let _ = park.recv();
        });
        let health = pf_zerocopy::zero_copy_health(IDENTITY.fetch_add(1, Relaxed));
        let shared = StreamShared::new(health);
        settle(&shared, at, hdr);
        let cap = PortalCapturer {
            queue: Default::default(),
            wake,
            poked: Default::default(),
            fence_wait: Default::default(),
            shared,
            cursor_slot: Default::default(),
            importer: Default::default(),
            resent_acked: 0,
            stall_since: None,
            vaapi_dmabuf: false,
            import_policy: Default::default(),
            raw_for_encoder: false,
            import_state: Default::default(),
            negotiation_confirmed: false,
            hdr_offer: false,
            hdr_source: crate::HdrSource::VirtualOutput,
            node_id: 7,
            quit: None,
            join: Some(join),
            keepalive: None,
            _portal: None,
            _gs_cursor: None,
        };
        Rig {
            parked: Some(parked),
            edge: Some(edge),
            cap,
        }
    }

    impl Rig {
        /// The loop thread's end: its wakeup sender goes with it.
        fn gone(mut self) -> Rig {
            self.edge = None;
            self
        }

        fn queue_frame(&self) {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: 1,
                height: 1,
                pts_ns: 1,
                format: PixelFormat::Bgrx,
                payload: FramePayload::Cpu(vec![0; 4]),
                cursor: None,
            };
            let mut q = self.cap.queue.lock().unwrap();
            q.push(frame, None, Instant::now());
        }
    }

    #[derive(Debug, PartialEq)]
    enum Latest {
        Frame,
        Idle,
        /// No frame, and the stall grace runs.
        Stalling,
        Stalled,
        /// Lost with the display still up.
        Lost,
        Ended,
    }

    fn latest(cap: &mut PortalCapturer) -> Latest {
        match cap.try_latest() {
            Ok(Some(_)) => Latest::Frame,
            Ok(None) if cap.stall_since.is_some() => Latest::Stalling,
            Ok(None) => Latest::Idle,
            Err(e) if e.downcast_ref::<crate::DisplayStillAlive>().is_some() => Latest::Lost,
            Err(e) if format!("{e}").contains("stalled") => Latest::Stalled,
            Err(e) if format!("{e}").contains("thread ended") => Latest::Ended,
            Err(e) => panic!("{e:#}"),
        }
    }

    /// `try_latest` once the stall grace has run out.
    fn latest_after_grace(cap: &mut PortalCapturer) -> Latest {
        cap.stall_since = Some(Instant::now() - Duration::from_secs(2));
        latest(cap)
    }

    #[derive(Debug, PartialEq)]
    enum First {
        Frame,
        Lost,
        NoBuffers,
        NoFormat,
        Ended,
    }

    const BUDGET: Duration = Duration::from_millis(300);

    /// The first-frame answer, and whether it waited out the budget.
    fn first(cap: &mut PortalCapturer) -> (First, bool) {
        let t0 = Instant::now();
        let first = match cap.frame_within(BUDGET, TimeoutVerdict::Conclusive) {
            Ok(_) => First::Frame,
            Err(e) => {
                let e = format!("{e}");
                if e.contains("zero-copy GPU import lost") {
                    First::Lost
                } else if e.contains("format negotiated but no buffers") {
                    First::NoBuffers
                } else if e.contains("format negotiation never completed") {
                    First::NoFormat
                } else if e.contains("thread ended before a frame") {
                    First::Ended
                } else {
                    panic!("{e}")
                }
            }
        };
        (first, t0.elapsed() >= BUDGET)
    }

    /// Whether `wait_arrival` waits out a deadline with nothing queued.
    fn waits(cap: &mut PortalCapturer) -> bool {
        let t0 = Instant::now();
        cap.wait_arrival(t0 + Duration::from_millis(50));
        t0.elapsed() >= Duration::from_millis(50)
    }

    /// `try_latest`, past the grace, with the loop thread gone, `is_alive`, `wait_arrival`:
    /// a broken capture is lost before anything else, and only `Streaming` is alive.
    #[test]
    fn the_tick_readers_answer_by_the_stream_state() {
        use At::*;
        use Latest::*;
        #[rustfmt::skip]
        let rows = [
            (Fresh,              Stalling, Stalled, Ended, false, true),
            (Fixated,            Stalling, Stalled, Ended, false, true),
            (Streaming,          Idle,     Idle,    Ended, true,  true),
            (Driving,            Idle,     Idle,    Ended, true,  true),
            (Paused,             Stalling, Stalled, Ended, false, true),
            (ErroredUnfixated,   Stalling, Stalled, Ended, false, true),
            (Errored,            Stalling, Stalled, Ended, false, true),
            (Broken,             Lost,     Lost,    Lost,  false, false),
            (BrokenErrored,      Lost,     Lost,    Lost,  false, false),
            (StreamingUnfixated, Idle,     Idle,    Ended, true,  true),
        ];
        for (at, now, after_grace, gone, alive, wait) in rows {
            let mut r = rig(at, false);
            assert_eq!(latest(&mut r.cap), now, "{at:?}");
            assert_eq!(latest_after_grace(&mut r.cap), after_grace, "{at:?}");
            assert_eq!(r.cap.is_alive(), alive, "{at:?}");
            assert_eq!(waits(&mut r.cap), wait, "{at:?}");
            assert_eq!(latest(&mut r.gone().cap), gone, "{at:?}");
        }
    }

    /// A queued frame is served unless the capture is broken, and ends the stall grace.
    #[test]
    fn a_queued_frame_outranks_all_but_a_broken_capture() {
        for (at, want) in [
            (At::Paused, Latest::Frame),
            (At::Errored, Latest::Frame),
            (At::Broken, Latest::Lost),
        ] {
            let mut r = rig(at, false);
            r.queue_frame();
            assert_eq!(latest(&mut r.cap), want, "{at:?}");
            assert_eq!(r.cap.stall_since, None, "{at:?}");
        }
    }

    /// A capturer whose loop thread finished is not alive, whatever the stream said last.
    #[test]
    fn a_finished_loop_thread_is_not_alive() {
        let mut r = rig(At::Streaming, false);
        r.parked = None;
        let join = r.cap.join.as_ref().unwrap();
        while !join.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!r.cap.is_alive());
    }

    /// The first-frame wait: broken ends it at once, then a queued frame, then an errored
    /// stream at once, then the budget or the loop thread's end. The verdict follows the
    /// format, not the stream state.
    #[test]
    fn the_first_frame_wait_answers_by_the_stream_state() {
        use At::*;
        use First::*;
        #[rustfmt::skip]
        let rows = [
            (Fresh,              (NoFormat, true),   (Ended, false)),
            (Fixated,            (NoBuffers, true),  (Ended, false)),
            (Streaming,          (NoBuffers, true),  (Ended, false)),
            (Driving,            (NoBuffers, true),  (Ended, false)),
            (Paused,             (NoBuffers, true),  (Ended, false)),
            (ErroredUnfixated,   (NoFormat, false),  (NoFormat, false)),
            (Errored,            (NoBuffers, false), (NoBuffers, false)),
            (Broken,             (Lost, false),      (Lost, false)),
            (BrokenErrored,      (Lost, false),      (Lost, false)),
            (StreamingUnfixated, (NoFormat, true),   (Ended, false)),
        ];
        for (at, live, gone) in rows {
            assert_eq!(first(&mut rig(at, false).cap), live, "{at:?}");
            assert_eq!(first(&mut rig(at, false).gone().cap), gone, "{at:?}");
        }
        for (at, want) in [(Errored, Frame), (Broken, Lost), (Streaming, Frame)] {
            let mut r = rig(at, false);
            r.queue_frame();
            assert_eq!(
                first(&mut r.cap),
                (want, false),
                "{at:?} with a frame queued"
            );
        }
    }

    /// HDR metadata follows the fixated transfer function.
    #[test]
    fn colour_follows_the_fixated_format() {
        let hdr = |at, hdr| rig(at, hdr).cap.colour().hdr_meta.is_some();
        assert!(hdr(At::Streaming, true));
        assert!(hdr(At::Paused, true));
        assert!(!hdr(At::Streaming, false));
        assert!(!hdr(At::Fresh, true));
    }
}
