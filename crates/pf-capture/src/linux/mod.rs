//! Live capture: xdg ScreenCast portal (`ashpd`) → PipeWire (`pipewire`).
//!
//! Two dedicated threads because both stacks are thread-tied:
//! - **portal** — async ashpd handshake on a multi-thread tokio runtime
//!   (control plane, never per-frame), then parks on a oneshot so the
//!   `proxy` and its zbus connection stay alive. Ashpd's `Session` has
//!   no `Drop`; the compositor tears the cast down when that connection
//!   drops.
//! - **pipewire** — owns the `!Send` MainLoop/Stream and pumps frames.
//!
//! Frames leave through a queue that waits out each producer render
//! ([`pipewire::FrameQueue`]) plus a wakeup edge; the direct capturer's are
//! rendered on arrival and leave through a one-deep [`FrameSlot`]. Payload may
//! be packed RGB, NV12, YUV444, 10-bit PQ, or a dmabuf that never touches the
//! CPU. Size is the negotiated PipeWire format, not the portal hint. [`PortalCapturer`]'s `Drop` quits and joins the
//! pipewire thread; [`PortalSession`]'s `Drop` fires the portal oneshot
//! and waits bounded so the zbus drop ends the ScreenCast.

use super::{CapturedFrame, Capturer, FramePayload, PixelFormat, ZeroCopyPolicy};
use anyhow::{anyhow, Context, Result};

// Gamescope's PipeWire node has no `SPA_META_Cursor`; this fills `cursor_live` from XFixes.
mod xfixes_cursor;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// One-deep overwriting mailbox: producer drops oldest. `sync_channel` is
/// drop-newest (`try_send` discards the fresh frame once full). A queued
/// [`CapturedFrame`] can own a dup'd dmabuf or CUDA buffer, so depth > 1
/// pins compositor buffers.
type FrameSlot = Arc<std::sync::Mutex<Option<CapturedFrame>>>;

/// `wait_arrival` for the direct capturer: returns once `slot` holds a frame,
/// `deadline` passes, or the producer is broken or gone. Never consumes: the
/// frame stays for `try_latest`, which also classifies a dead producer.
fn wait_for_frame(
    slot: &FrameSlot,
    wake: &Receiver<()>,
    broken: &AtomicBool,
    deadline: std::time::Instant,
) {
    if broken.load(Ordering::Relaxed) {
        return;
    }
    loop {
        if slot.lock().is_ok_and(|s| s.is_some()) {
            return;
        }
        let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return;
        };
        if wake.recv_timeout(left).is_err() {
            return;
        }
    }
}

/// Drains stale wakeup edges so the next [`wait_for_frame`] cannot return
/// early. `true` when the producer thread is gone (its sender dropped).
fn drain_edges(wake: &Receiver<()>) -> bool {
    loop {
        match wake.try_recv() {
            Ok(()) => continue,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
}

/// Named bools: four adjacent same-typed args transpose silently and
/// negotiate the wrong pod family (black screen).
#[derive(Clone, Copy)]
struct CaptureOpts {
    /// `false` forces CPU mmap even when `PUNKTFUNK_ZEROCOPY` is set — the
    /// session plan does that when 4:4:4 has no zero-copy convert (`SessionPlan::output_format`).
    allow_zerocopy: bool,
    /// Tiled dmabufs convert via `ImportKind::Tiled444`, not NV12/RGB.
    want_444: bool,
    /// Offer only 10-bit PQ/BT.2020 as LINEAR dmabufs. SHM cannot: Mutter's
    /// SHM path paints 8-bit ARGB32, and the tiled EGL blit is 8-bit.
    want_hdr: bool,
    /// 10-bit SDR: keep packed RGB (skip the NV12 convert) so direct-NVENC widens 8→10. A
    /// planar 8-bit surface fails a 10-bit NVENC session; packed RGB is the only 8-bit input
    /// it accepts there.
    ten_bit_sdr: bool,
    /// The producer offers 10-bit SDR (gamescope from `+pfhdr26`): its P010 and packed 10-bit
    /// pods go out under BT.709 ahead of the 8-bit ones, which stay as the fallback.
    sdr10_native: bool,
    /// Skip buffers until negotiated size matches `preferred` — KWin virtual
    /// outputs birth a sacrificial mode then renegotiate (`kwin.rs` `create`).
    /// `false` elsewhere: Mutter sizes from negotiation; gamescope fixates.
    expect_exact_dims: bool,
    /// `true` (KWin): `id == 0` means pointer hidden — producer rewrites
    /// `SPA_META_Cursor` every buffer. `false` (Mutter): buffers recycle
    /// the region. See [`pw_cursor::CursorState::id0_hides`].
    cursor_id0_hides: bool,
    /// Gamescope omits cursor metadata. Its proved tiled formats lead; LINEAR remains fallback.
    producer_is_gamescope: bool,
    /// Least dmabuf pool depth to ask for: [`crate::POOL_MIN`], or
    /// [`crate::KWIN_POOL_MIN`] so KWin's default of 3 cannot win.
    pool_min: i32,
    /// Deepest pool the producer serves ([`crate::KWIN_POOL_MAX`]); `None` serves any depth.
    /// A deeper ask for the raw lane stops here.
    pool_max: Option<i32>,
    /// Offer `maxFramerate = 0/1` so KWin records on its own frame signal
    /// rather than a millisecond-rounded timer. KWin only; see
    /// [`crate::unpaced_capture`].
    unpaced: bool,
    /// Drive the producer as a PipeWire lazy driver when it emits RequestProcess
    /// (Mutter ≥ 49 virtual monitors): it then paints when it asks, one paint per
    /// wire interval at most. See [`crate::lazy_capture`].
    lazy: bool,
}

#[derive(Clone)]
struct CaptureSignals {
    /// Failure memory scoped to this producer identity and shared with encoder frames.
    health: pf_zerocopy::ZeroCopyHealth,
    /// Per-frame de-pad runs only while set; pooling a 5K capturer is cheap
    /// between streams.
    active: Arc<AtomicBool>,
    /// Format agreed. Timeout diagnosis: mismatch vs idle/unmapped compositor.
    negotiated: Arc<AtomicBool>,
    /// Stream is `Streaming`. Distinguishes a static desktop from a dead source.
    streaming: Arc<AtomicBool>,
    /// This stream drives the graph: the producer paints only in cycles the
    /// loop thread's pacer starts on its requests. Cleared with `streaming`.
    driving: Arc<AtomicBool>,
    /// GPU import is gone for this stream (worker death, or a tiled or planar
    /// frame the CPU de-pad cannot read). Never cleared.
    broken: Arc<AtomicBool>,
    /// The stream reached `Error` (e.g. "no more input formats"). Terminal: it never delivers.
    errored: Arc<AtomicBool>,
    /// Buffers the producer sent again while this capture still held them (PipeWire < 1.6,
    /// no `node.reliable`). Each is re-held under a new generation; the count rides the
    /// provenance line, and [`Capturer::take_reference_risk`] turns a step into one IDR.
    resent: Arc<std::sync::atomic::AtomicU64>,
    hdr_negotiated: Arc<AtomicBool>,
    /// Thread actually advertised the EGL→CUDA dmabuf-only offer. `plan.build_importer`
    /// is not enough: a failed importer means no dmabuf was offered, so a
    /// timeout must not latch the GPU offer off.
    gpu_dmabuf_offer: Arc<AtomicBool>,
    /// Overlay from every buffer's `SPA_META_Cursor`, including cursor-only
    /// buffers that never become frames. Gamescope XFixes publishes here too.
    cursor_live: Arc<std::sync::Mutex<Option<pf_frame::CursorOverlay>>>,
    /// The host forwards or blends [`Self::cursor_live`] itself, so the CPU copy must not
    /// bake the pointer into its pixels as well. Set by [`Capturer::set_cursor_forward`].
    host_places_cursor: Arc<AtomicBool>,
    /// Packed `(w << 32) | h`; `0` until `param_changed`. Gamescope cursor
    /// maps root-space into frame space (`-w/-h` vs `-W/-H` are independent).
    frame_size: Arc<std::sync::atomic::AtomicU64>,
    /// NVIDIA zero-copy importer, usually the isolated worker. The loop thread
    /// fills it after negotiation; the consumer imports held frames through it
    /// ([`PortalCapturer::import_held`]) and retires it on a LINEAR failure.
    importer: Arc<std::sync::Mutex<Option<pf_zerocopy::Importer>>>,
    /// `importer` is `Some`. Read per frame on the loop thread without the lock.
    has_importer: Arc<AtomicBool>,
    /// `importer` runs in this process, its GL context current on the loop thread only. Read per
    /// frame without the lock, which the consumer holds across a whole worker import.
    importer_in_process: Arc<AtomicBool>,
}

/// Producer identity plus the consumer policy whose failures must stay independent.
fn health_identity(base: u64, policy: &ZeroCopyPolicy) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    base.hash(&mut hash);
    policy.backend_is_vaapi.hash(&mut hash);
    policy.pyrowave_session.hash(&mut hash);
    policy.nvenc_raw_dmabuf.hash(&mut hash);
    policy.encoder_modifiers.hash(&mut hash);
    hash.finish()
}

impl CaptureSignals {
    fn new(health: pf_zerocopy::ZeroCopyHealth) -> Self {
        Self {
            health,
            active: Arc::new(AtomicBool::new(false)),
            negotiated: Arc::new(AtomicBool::new(false)),
            streaming: Arc::new(AtomicBool::new(false)),
            driving: Arc::new(AtomicBool::new(false)),
            broken: Arc::new(AtomicBool::new(false)),
            errored: Arc::new(AtomicBool::new(false)),
            resent: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            hdr_negotiated: Arc::new(AtomicBool::new(false)),
            gpu_dmabuf_offer: Arc::new(AtomicBool::new(false)),
            cursor_live: Arc::new(std::sync::Mutex::new(None)),
            host_places_cursor: Arc::new(AtomicBool::new(false)),
            frame_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            importer: Arc::new(std::sync::Mutex::new(None)),
            has_importer: Arc::new(AtomicBool::new(false)),
            importer_in_process: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Portal + PipeWire capturer, reused across streams. [`set_active`] gates
/// the per-frame de-pad so the screencast stays up between reconnects.
pub struct PortalCapturer {
    /// Arrivals awaiting their renders. The wait on a render runs on this side, never on
    /// the PipeWire thread: that one hands the producer's buffers back.
    queue: pipewire::FrameQueue,
    /// Wakeup only — the queue holds the frames, so a coalesced edge loses
    /// nothing. Sender dies with the PipeWire thread (`Disconnected`).
    wake: Receiver<()>,
    /// Arrival to take, per frame (`PUNKTFUNK_PERF`).
    fence_wait: pipewire::FenceWaitStats,
    signals: CaptureSignals,
    /// `signals.resent` as of the last [`Capturer::take_reference_risk`].
    resent_acked: u64,
    /// First drop out of `Streaming` with no frame. Grace for a transient
    /// renegotiation; cleared on a frame or when `Streaming` again.
    stall_since: Option<std::time::Instant>,
    /// Raw-dmabuf passthrough offer, copied from the thread's
    /// [`NegotiationPlan`](pipewire::plan::NegotiationPlan) — never re-derived.
    /// A failed offer latches this capture's [`pf_zerocopy::ZeroCopyHealth`].
    vaapi_dmabuf: bool,
    /// CUDA import choices for held frames ([`Self::import_held`]).
    import_policy: pipewire::ImportPolicy,
    /// Held frames pass through as dmabufs: the NVENC encoder's worker converts them. A
    /// failed offer latches the same raw-dmabuf latch the VAAPI passthrough uses.
    raw_for_encoder: bool,
    /// This consumer's import memory (LINEAR NV12 latch, tiled failure streak).
    import_state: pipewire::ImportState,
    /// One-shot: this capture's dmabuf offer negotiated; retry budget credited.
    negotiation_confirmed: bool,
    /// HDR offer. A failed negotiation latches SDR for [`Self::hdr_source`]
    /// only, not process-wide.
    hdr_offer: bool,
    /// Latch target for a failed [`hdr_offer`](Self::hdr_offer). See [`super::HdrSource`].
    hdr_source: super::HdrSource,
    node_id: u32,
    /// `Drop` sends this. Without it `mainloop.run()` blocks until process
    /// exit (leaks the thread and EGL/CUDA). `Option` so `Drop` can take it.
    quit: Option<::pipewire::channel::Sender<()>>,
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
    /// ([`crate::portal_rt::negotiate_cursor_mode`]).
    pub fn open(
        anchored: bool,
        want_hdr: bool,
        want_metadata_cursor: bool,
        policy: ZeroCopyPolicy,
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
            },
            policy,
        )?
        .into_capturer(node_id, None, Some(portal), super::HdrSource::PortalMonitor))
    }

    /// Capturer for an already-created virtual output's PipeWire node.
    /// `opts.producer` supplies the producer contracts a node id cannot reveal.
    /// `keepalive` releases the output with the capturer.
    pub fn from_virtual_output(
        remote_fd: Option<OwnedFd>,
        node_id: u32,
        preferred_mode: Option<(u32, u32, u32)>,
        keepalive: Box<dyn Send>,
        opts: super::VirtualOutputOpts,
    ) -> Result<PortalCapturer> {
        let kwin = opts.producer == super::Producer::Kwin;
        let capture = CaptureOpts {
            allow_zerocopy: opts.allow_zerocopy,
            want_444: opts.want_444,
            want_hdr: opts.want_hdr,
            ten_bit_sdr: opts.ten_bit_sdr,
            sdr10_native: opts.sdr10_native,
            expect_exact_dims: opts.expect_exact_dims,
            cursor_id0_hides: kwin,
            producer_is_gamescope: opts.producer == super::Producer::Gamescope,
            pool_min: if kwin {
                crate::KWIN_POOL_MIN
            } else {
                crate::POOL_MIN
            },
            pool_max: kwin.then_some(crate::KWIN_POOL_MAX),
            unpaced: kwin && crate::unpaced_capture(),
            lazy: crate::lazy_capture(),
        };
        tracing::info!(
            node_id,
            allow_zerocopy = capture.allow_zerocopy,
            want_444 = capture.want_444,
            want_hdr = capture.want_hdr,
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
        Ok(
            spawn_pipewire(remote_fd, node_id, preferred_mode, capture, opts.policy)?
                .into_capturer(
                    node_id,
                    Some(keepalive),
                    None,
                    super::HdrSource::VirtualOutput,
                ),
        )
    }
}

struct PwHandles {
    queue: pipewire::FrameQueue,
    wake: Receiver<()>,
    signals: CaptureSignals,
    vaapi_dmabuf: bool,
    hdr_offer: bool,
    /// Copied from the plan: what the consumer's import of a held frame does.
    import_policy: pipewire::ImportPolicy,
    /// Held frames pass through as dmabufs: the NVENC encoder converts them itself.
    raw_for_encoder: bool,
    quit: ::pipewire::channel::Sender<()>,
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
        hdr_source: super::HdrSource,
    ) -> PortalCapturer {
        PortalCapturer {
            queue: self.queue,
            wake: self.wake,
            fence_wait: Default::default(),
            signals: self.signals,
            resent_acked: 0,
            stall_since: None,
            vaapi_dmabuf: self.vaapi_dmabuf,
            import_policy: self.import_policy,
            raw_for_encoder: self.raw_for_encoder,
            import_state: pipewire::ImportState::default(),
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

/// Spawn one PipeWire consumer with health keyed by source and encoder policy.
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
    let queue = pipewire::FrameQueue::default();
    let queue_cb = queue.clone();
    let (wake_tx, wake_rx) = sync_channel::<()>(1);
    // Portal-fd vs virtual-output with the same node number are different sources.
    let identity = u64::from(node_id) | (u64::from(fd.is_some()) << 32);
    let signals = CaptureSignals::new(pf_zerocopy::zero_copy_health(health_identity(
        identity, &policy,
    )));
    let signals_cb = signals.clone();
    // Absolute `::pipewire`: inner `mod pipewire` shadows the crate. Receiver
    // attaches to the loop; sender fires in `Drop`.
    let (quit_tx, quit_rx) = ::pipewire::channel::channel::<()>();
    let zerocopy = allow_zerocopy && pf_zerocopy::enabled();
    // HDR cannot ride SHM: FORCE_SHM drops the HDR offer (SDR, loudly).
    // Shared parser, not `== "1"` — a bare compare ignored `true`/`on`/`yes`.
    let force_shm = pf_host_config::env_on("PUNKTFUNK_FORCE_SHM").unwrap_or(false);
    let want_hdr = if want_hdr && force_shm {
        tracing::warn!(
            "HDR capture requested but PUNKTFUNK_FORCE_SHM=1 — the SHM path is 8-bit only; \
             offering SDR"
        );
        false
    } else {
        want_hdr
    };
    // Resolved once and handed to the thread; every env/latch read happens here.
    let plan = pipewire::negotiation_plan(pipewire::NegotiationInputs {
        zerocopy,
        force_shm,
        want_hdr,
        // SHM is 8-bit, like HDR above.
        sdr10_native: sdr10_native && !force_shm,
        want_444,
        backend_is_vaapi: policy.backend_is_vaapi,
        pyrowave_session: policy.pyrowave_session,
        native_nv12_session: policy.native_nv12_session,
        raw_dmabuf_import_disabled: signals.health.raw_disabled(),
        gpu_import_disabled: signals.health.gpu_import_disabled(),
        gpu_dmabuf_negotiation_failed: signals.health.gpu_negotiation_disabled(),
        // Default ON; `=0` (any falsy spelling, shared parser) restores packed RGB.
        native_nv12_env_on: pf_host_config::env_on("PUNKTFUNK_PIPEWIRE_NV12").unwrap_or(true),
        planar_refused: signals.health.planar_refused(),
        hdr_cuda_ok: policy.hdr_cuda_ok,
        nv12_env_on: pf_zerocopy::nv12_enabled(),
        // A tiled refusal keeps NVENC on the CUDA import, as on the direct path: the raw lane
        // would be offered the same tiled modifiers again.
        nvenc_raw: policy.nvenc_raw_dmabuf && !signals.health.passthrough_tiled_refused(),
    });
    let vaapi_dmabuf = plan.vaapi_passthrough;
    let import_policy = plan.import_policy;
    let raw_for_encoder = plan.nvenc_raw;
    let join = thread::Builder::new()
        .name("punktfunk-pipewire".into())
        .spawn(move || {
            if let Err(e) = pipewire::pipewire_thread(
                fd,
                node_id,
                queue_cb,
                wake_tx,
                signals_cb,
                plan,
                // `allow_zerocopy` is already in `plan`; `want_hdr` may have been cleared by FORCE_SHM.
                CaptureOpts { want_hdr, ..opts },
                preferred,
                quit_rx,
                policy,
            ) {
                tracing::error!(error = %format!("{e:#}"), "pipewire capture thread failed");
            }
        })
        .context("spawn pipewire thread")?;
    Ok(PwHandles {
        queue,
        wake: wake_rx,
        signals,
        vaapi_dmabuf,
        hdr_offer: want_hdr,
        import_policy,
        raw_for_encoder,
        quit: quit_tx,
        join,
    })
}

impl Capturer for PortalCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        self.frame_within(Duration::from_secs(10), TimeoutVerdict::Conclusive)
    }

    fn cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        // Includes cursor-only buffers. Gamescope fills this via XFixes.
        self.signals
            .cursor_live
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
    }

    fn set_cursor_forward(&mut self, _on: bool) {
        // Either way the host places the pointer from `cursor()`: forwarded, or on the frame
        // for the encoder blend.
        self.signals
            .host_places_cursor
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn attach_gamescope_cursor(&mut self, targets: crate::GamescopeCursorTargets) {
        // Gamescope paints no `SPA_META_Cursor`. Idempotent: do not `spawn`
        // before dropping the old source (two publishers, or a `None` spawn
        // destroying a working reader).
        if self._gs_cursor.is_some() {
            return;
        }
        self._gs_cursor = xfixes_cursor::XFixesCursorSource::spawn(
            targets,
            Arc::clone(&self.signals.cursor_live),
            Arc::clone(&self.signals.frame_size),
        );
    }

    fn next_frame_within(&mut self, budget: Duration) -> Result<CapturedFrame> {
        self.frame_within(budget, TimeoutVerdict::Conclusive)
    }

    fn next_frame_within_provisional(&mut self, budget: Duration) -> Result<CapturedFrame> {
        // Truncated first attempt: expiry re-runs the schedule, does not
        // convict an offer (`TimeoutVerdict`).
        self.frame_within(budget, TimeoutVerdict::Provisional)
    }

    fn supports_arrival_wait(&self) -> bool {
        true
    }

    fn wait_arrival(&mut self, deadline: std::time::Instant) {
        // A driven producer paints on its own requests (`pipewire::pacer::Pacer`), so
        // this wait never triggers a paint. It returns on a finished render, not on an
        // arrival: the producer sends a buffer before it has painted it.
        if !self.signals.broken.load(Ordering::Relaxed) {
            let _ = pipewire::wait_ready(&self.queue, &self.wake, deadline);
        }
    }

    /// Only the virtual-output path holds one; the portal thread owns its own session.
    fn take_keepalive(&mut self) -> Option<Box<dyn Send>> {
        self.keepalive.take()
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        if self.signals.broken.load(Ordering::Relaxed) {
            return Err(anyhow!(
                "zero-copy GPU import lost (node {}): the import worker died, or a tiled or \
                 planar offer failed — rebuilding capture",
                self.node_id
            )
            .context(super::DisplayStillAlive));
        }
        // A dead thread's leftover frame is still served first.
        let producer_gone = drain_edges(&self.wake);
        let latest = self.take_frame();
        if producer_gone && latest.is_none() {
            return Err(anyhow!("PipeWire capture thread ended"));
        }
        if latest.is_some() || self.signals.streaming.load(Ordering::Relaxed) {
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

    fn set_active(&mut self, active: bool) {
        self.signals.active.store(active, Ordering::Relaxed);
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

    /// Sticky terminal states, no frame consumed. Thread-exited is otherwise
    /// indistinguishable from idle (`streaming` keeps its last value). A
    /// static desktop stays `Streaming` (no buffers) and is not reported dead.
    fn is_alive(&self) -> bool {
        !self.signals.broken.load(Ordering::Relaxed)
            && self.signals.streaming.load(Ordering::Relaxed)
            && self.join.as_ref().is_some_and(|j| !j.is_finished())
    }

    /// A re-sent buffer since the last call: the frame encoded from the earlier read may be
    /// torn, so the loop refreshes the reference with one IDR.
    fn take_reference_risk(&mut self) -> bool {
        let n = self.signals.resent.load(Ordering::Relaxed);
        let risk = n != self.resent_acked;
        self.resent_acked = n;
        risk
    }

    /// Standard HDR10 default block once 10-bit PQ negotiated. Neither Linux
    /// producer exposes mastering through the screencast (Mutter has none;
    /// gamescope's `VK_EXT_hdr_metadata` stops at the compositor). The native
    /// loop prefers the client's volume when sent (`Hello::display_hdr`).
    fn hdr_meta(&self) -> Option<pf_frame::HdrMeta> {
        if !self.signals.hdr_negotiated.load(Ordering::Relaxed) {
            return None;
        }
        Some(pf_frame::HdrMeta {
            // ST.2086 order G, B, R; (x, y) chromaticity in 1/50000 units.
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],                 // D65
            max_display_mastering_luminance: 10_000_000, // 1000 cd/m² (0.0001 units)
            min_display_mastering_luminance: 50,         // 0.005 cd/m²
            max_cll: 0,
            max_fall: 0,
        })
    }
}

/// Whether an expired first-frame budget may convict an offer. The retry
/// loop's truncated first attempt is `Provisional`: expiry means the
/// schedule moved on, not that the compositor refused. Only a full-length
/// wait may latch — a gamescope cold start needs that extra window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutVerdict {
    Conclusive,
    Provisional,
}

/// Offer a first-frame timeout implicates. Split out so the latch policy
/// is testable. A negotiated format clears every offer; forced
/// `PUNKTFUNK_ZEROCOPY=1` keeps both dmabuf arms erroring (operator asked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutOffer {
    /// Format negotiated; compositor produced no buffers.
    NoBuffers,
    Hdr,
    RawDmabuf,
    GpuDmabuf,
    /// Nothing negotiated — format/modifier mismatch.
    NoFormat,
}

fn classify_first_frame_timeout(
    negotiated: bool,
    hdr_offer: bool,
    vaapi_dmabuf: bool,
    gpu_dmabuf_offer: bool,
    zerocopy_forced: bool,
) -> TimeoutOffer {
    if negotiated {
        TimeoutOffer::NoBuffers
    } else if hdr_offer {
        TimeoutOffer::Hdr
    } else if vaapi_dmabuf && !zerocopy_forced {
        TimeoutOffer::RawDmabuf
    } else if gpu_dmabuf_offer && !zerocopy_forced {
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

impl PortalCapturer {
    /// First frame can lag negotiation; later frames arrive at ~fps. Wait in
    /// 500 ms slices so a GPU-import poison or an errored stream fails within
    /// ~0.5 s instead of the full first-frame budget.
    fn frame_within(&mut self, budget: Duration, verdict: TimeoutVerdict) -> Result<CapturedFrame> {
        let started = std::time::Instant::now();
        let deadline = started + budget;
        loop {
            if self.signals.broken.load(Ordering::Relaxed) {
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
            if self.signals.errored.load(Ordering::Relaxed) {
                let spent = started.elapsed();
                return self.next_frame_timed_out(RecvTimeoutError::Timeout, spent, verdict);
            }
            let slice = Duration::from_millis(500)
                .min(deadline.saturating_duration_since(std::time::Instant::now()));
            let until = std::time::Instant::now() + slice;
            let end = if !pipewire::wait_ready(&self.queue, &self.wake, until) {
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
        let cell = self.signals.importer.clone();
        let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
        let importer = guard.as_mut()?;
        let plane = pf_zerocopy::DmabufPlane {
            fd: std::os::fd::AsRawFd::as_raw_fd(&held.fd),
            offset: held.offset,
            stride: held.stride,
        };
        match pipewire::gpu_import(
            importer,
            self.import_policy,
            &mut self.import_state,
            &self.signals,
            format,
            width,
            height,
            plane,
            held.modifier,
        ) {
            pipewire::ImportOutcome::Frame(buf, format) => Some(CapturedFrame {
                width,
                height,
                pts_ns,
                format,
                payload: FramePayload::Cuda(buf),
                cursor,
                provenance,
            }),
            pipewire::ImportOutcome::Dropped => None,
            pipewire::ImportOutcome::ImporterLost => {
                *guard = None;
                self.signals.has_importer.store(false, Ordering::Relaxed);
                None
            }
        }
    }

    /// Credit the dmabuf negotiation retry budget. Once per capture: the
    /// budget counts consecutive failed builds, not frames.
    fn note_negotiation_confirmed(&mut self) {
        if (self.vaapi_dmabuf || self.raw_for_encoder) && !self.negotiation_confirmed {
            self.negotiation_confirmed = true;
            self.signals.health.note_raw_negotiation_ok();
        }
    }

    /// Budget expired or the thread ended. Latch the sticky downgrade only
    /// when the expiry convicts the offer ([`timeout_convicts`]).
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
                    self.signals.negotiated.load(Ordering::Relaxed),
                    self.hdr_offer,
                    self.vaapi_dmabuf,
                    self.signals.gpu_dmabuf_offer.load(Ordering::Relaxed),
                    pf_zerocopy::zerocopy_forced(),
                );
                let convicted = timeout_convicts(offer, verdict);
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
                            super::note_hdr_capture_failed(self.hdr_source);
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
                    TimeoutOffer::RawDmabuf => {
                        // Latch is scoped to raw-passthrough. Feeding
                        // `pf_zerocopy::enabled()` dropped every later
                        // session (NVENC EGL→CUDA included) to CPU capture.
                        if convicted {
                            self.signals.health.note_raw_negotiation_failed();
                        }
                        Err(anyhow!(
                            "no PipeWire frame within {within}s (node {}): the compositor never \
                             accepted the dmabuf-only offer (raw-dmabuf passthrough){}",
                            self.node_id,
                            if convicted {
                                " — downgrading THIS path to CPU capture for the rest of the \
                                 process; the pipeline rebuild will renegotiate without dmabuf"
                            } else {
                                sentence
                            }
                        ))
                    }
                    TimeoutOffer::GpuDmabuf => {
                        // One full-length timeout is conclusive: a compositor
                        // that allocates none of the importer's modifiers
                        // refuses them identically on every retry. Forced
                        // `PUNKTFUNK_ZEROCOPY=1` keeps erroring (same as raw).
                        if convicted {
                            self.signals.health.note_gpu_negotiation_failed();
                        }
                        Err(anyhow!(
                            "no PipeWire frame within {within}s (node {}): the compositor never \
                             accepted the dmabuf-only offer (EGL→CUDA GPU import){}",
                            self.node_id,
                            if convicted {
                                " — downgrading THIS offer to the CPU path for the rest of the \
                                 process; the pipeline rebuild will renegotiate without dmabuf"
                            } else {
                                sentence
                            }
                        ))
                    }
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

/// Direct `ext-image-copy-capture-v1` capturer: the compositor fills buffers we
/// allocate, and we re-arm the instant one is ready.
///
/// Shares the portal's one-deep mailbox and wakeup edge, so the encode loop's
/// arrival wait is unchanged. What it does not share is any pacing: there is no
/// portal round trip and no PipeWire graph, so the rate is the compositor's
/// repaint rate (`design/linux-consumer-driven-capture.md` §8).
pub struct WlCapturer {
    slot: FrameSlot,
    wake: Receiver<()>,
    signals: CaptureSignals,
    output_name: String,
    quit: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
    /// Holds the compositor output; dropped after the thread is joined, unless a
    /// capture-only rebuild took it back (`take_keepalive`).
    keepalive: Option<Box<dyn Send>>,
}

impl WlCapturer {
    /// Open the named compositor output with identity-scoped failure health.
    /// Missing protocol/output or no consumer-importable dmabuf keeps the portal path,
    /// so a failure hands `keepalive` back for it.
    pub fn open(
        output_name: String,
        keepalive: Box<dyn Send>,
        policy: ZeroCopyPolicy,
    ) -> std::result::Result<WlCapturer, (anyhow::Error, Box<dyn Send>)> {
        let slot: FrameSlot = Arc::new(std::sync::Mutex::new(None));
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        output_name.hash(&mut hash);
        let identity = health_identity(hash.finish() | (1 << 63), &policy);
        let signals = CaptureSignals::new(pf_zerocopy::zero_copy_health(identity));
        signals.active.store(true, Ordering::Relaxed);
        let h = match wl_capture::spawn(output_name.clone(), policy, slot, signals) {
            Ok(h) => h,
            Err(e) => return Err((e, keepalive)),
        };
        Ok(WlCapturer {
            slot: h.slot,
            wake: h.wake,
            signals: h.signals,
            output_name,
            quit: h.quit,
            join: Some(h.join),
            keepalive: Some(keepalive),
        })
    }

    fn take_frame(&self) -> Option<CapturedFrame> {
        self.slot.lock().ok().and_then(|mut s| s.take())
    }
}

impl Capturer for WlCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        self.next_frame_within(Duration::from_secs(10))
    }

    fn next_frame_within(&mut self, budget: Duration) -> Result<CapturedFrame> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if let Some(f) = self.take_frame() {
                return Ok(f);
            }
            if self.signals.broken.load(Ordering::Relaxed) {
                return Err(anyhow!(
                    "direct wayland capture failed on output {}",
                    self.output_name
                ));
            }
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return Err(anyhow!(
                    "no frame within {:.1}s from the direct wayland capture on output {} — the \
                     compositor accepted the session but painted nothing",
                    budget.as_secs_f32(),
                    self.output_name
                ));
            };
            if self
                .wake
                .recv_timeout(left.min(Duration::from_millis(500)))
                .is_err()
                && !self.signals.streaming.load(Ordering::Relaxed)
            {
                return Err(anyhow!("direct wayland capture thread ended"));
            }
        }
    }

    fn supports_arrival_wait(&self) -> bool {
        true
    }

    fn wait_arrival(&mut self, deadline: std::time::Instant) {
        wait_for_frame(&self.slot, &self.wake, &self.signals.broken, deadline);
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        if self.signals.broken.load(Ordering::Relaxed) {
            return Err(anyhow!(
                "direct wayland capture lost on output {} — rebuilding capture",
                self.output_name
            ));
        }
        // A thread that ended without a failure (the compositor stopped the
        // session) fails here too, after its leftover frame, so the loop rebuilds.
        let producer_gone = drain_edges(&self.wake);
        let latest = self.take_frame();
        if producer_gone && latest.is_none() {
            return Err(anyhow!("direct wayland capture thread ended"));
        }
        Ok(latest)
    }

    fn cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        self.signals.cursor_live.lock().ok().and_then(|c| c.clone())
    }

    fn is_alive(&self) -> bool {
        !self.signals.broken.load(Ordering::Relaxed)
            && self.join.as_ref().is_some_and(|j| !j.is_finished())
    }

    fn take_keepalive(&mut self) -> Option<Box<dyn Send>> {
        self.keepalive.take()
    }
}

impl Drop for WlCapturer {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for PortalCapturer {
    fn drop(&mut self) {
        // Quit then join before keepalive drops: releases EGL/CUDA, then
        // the virtual output. Without this `mainloop.run()` blocks until
        // process exit. `send` errs only if the thread already exited.
        if let Some(quit) = self.quit.take() {
            let _ = quit.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

// ScreenCast/RemoteDesktop handshake + GNOME colour-mode probe. Async,
// not per-frame. `gnome_hdr_monitor_active` is re-exported from `lib.rs`.
mod portal;
pub use portal::gnome_hdr_monitor_active;
use portal::portal_thread;

// PipeWire consumer (`!Send`, owns its thread). Inner `mod pipewire` shadows
// the crate, hence `::pipewire` in this file.
mod pipewire;
// Client-allocated dmabufs for the direct capture path.
mod gbm_pool;
// Direct `ext-image-copy-capture-v1` capture, with no portal and no PipeWire.
mod wl_capture;
// Negotiation POD builders and cursor-meta parser + CPU blits. Pure enough
// to unit-test without a compositor.
mod pw_cursor;
mod pw_pods;
// Explicit sync (`SPA_META_SyncTimeline`): syncobj waits and release signals.
mod sync_timeline;

#[cfg(test)]
mod first_frame_timeout_tests {
    use super::{classify_first_frame_timeout, timeout_convicts, TimeoutOffer, TimeoutVerdict};

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

    #[test]
    fn classification_mirrors_the_negotiation_state_precedence() {
        assert_eq!(
            classify_first_frame_timeout(true, true, true, true, false),
            TimeoutOffer::NoBuffers
        );
        // HDR outranks the dmabuf arms — it is the offer that failed.
        assert_eq!(
            classify_first_frame_timeout(false, true, true, true, false),
            TimeoutOffer::Hdr
        );
        assert_eq!(
            classify_first_frame_timeout(false, false, true, true, false),
            TimeoutOffer::RawDmabuf
        );
        assert_eq!(
            classify_first_frame_timeout(false, false, false, true, false),
            TimeoutOffer::GpuDmabuf
        );
        assert_eq!(
            classify_first_frame_timeout(false, false, false, false, false),
            TimeoutOffer::NoFormat
        );
    }

    #[test]
    fn a_forced_zerocopy_keeps_both_dmabuf_arms_erroring_loudly_instead_of_implicated() {
        // `PUNKTFUNK_ZEROCOPY=1` is the operator insisting on the path —
        // timeout falls through to the generic diagnosis and never latches.
        assert_eq!(
            classify_first_frame_timeout(false, false, true, true, true),
            TimeoutOffer::NoFormat
        );
    }
}

#[cfg(test)]
mod wl_capturer_tests {
    use super::{CaptureSignals, Capturer, WlCapturer};
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::sync_channel;
    use std::sync::Arc;

    /// A thread that ended cleanly drops its wake sender. `try_latest` must say so,
    /// or the encode loop repeats the last frame and never rebuilds capture.
    #[test]
    fn try_latest_reports_an_ended_thread() {
        let (edge, wake) = sync_channel::<()>(1);
        let mut cap = WlCapturer {
            slot: Arc::default(),
            wake,
            signals: CaptureSignals::new(pf_zerocopy::zero_copy_health(u64::MAX)),
            output_name: "TEST-1".into(),
            quit: Arc::new(AtomicBool::new(false)),
            join: None,
            keepalive: None,
        };
        edge.try_send(()).expect("empty channel");
        assert!(
            matches!(cap.try_latest(), Ok(None)),
            "a live thread with no frame is idle"
        );
        drop(edge);
        assert!(cap.try_latest().is_err());
    }
}
