//! Display + capture + encoder bring-up for one session: [`build_pipeline`] and its bounded
//! retry, the Welcome-time [`prepare_display`], and [`open_session_encoder`], which every
//! rebuild path (mode switch, capture loss, bitrate fallback) goes through.

use super::*;
#[cfg(target_os = "linux")]
use crate::capture::OutputLease;
use crate::native::bitrate::{pyrowave_mode_kbps, resolve_bitrate_kbps_for, EncDerive};

/// One built pipeline. `bitrate_kbps` is the rate the encoder actually opened at.
pub(in crate::native) struct Pipeline {
    pub(super) capturer: Box<dyn crate::capture::Capturer>,
    pub(super) enc: Box<dyn crate::encode::Encoder>,
    pub(super) frame: crate::capture::CapturedFrame,
    pub(super) interval: std::time::Duration,
    pub(super) node_id: u32,
    pub(super) display_gen: Option<u64>,
    pub(super) bitrate_kbps: u32,
    /// What the encoder takes of each captured picture.
    pub(super) reframe: punktfunk_core::video_fit::Reframe,
    /// The output this capture attached to, for a capture-only rebuild. `None` where the
    /// capture cannot re-attach (a portal remote fd is spent on the first connect).
    #[cfg(target_os = "linux")]
    pub(super) lease: Option<OutputLease>,
    /// The display's refresh period when it runs faster than the stream: its frames' stamps
    /// sit on that finer tick and are corrected onto the content's cadence ([`panel_tick_ns`]).
    #[cfg(target_os = "linux")]
    pub(super) panel_tick_ns: Option<u64>,
}

/// The display's refresh period in ns when it runs faster than the `stream_hz` it serves.
#[cfg(target_os = "linux")]
pub(super) fn panel_tick_ns(display_hz: u32, stream_hz: u32) -> Option<u64> {
    (display_hz > stream_hz).then(|| 1_000_000_000 / u64::from(display_hz))
}

/// Display + pipeline built on the prep thread while the Start round trip is in flight.
pub(in crate::native) struct PreparedDisplay {
    pub(super) vd: Box<dyn crate::vdisplay::VirtualDisplay>,
    pub(super) pipeline: Pipeline,
}

/// Prep thread: sender delivers [`SessionContext`]; drop un-received aborts into keep-alive.
pub(in crate::native) type PrepHandle = (
    std::sync::mpsc::SyncSender<SessionContext>,
    std::thread::JoinHandle<Result<()>>,
);

/// What every pipeline build of one session asks for. A rebuild moves a field or two: the
/// mode, the rate, the wire index its encoder continues at.
#[derive(Clone)]
pub(super) struct BuildRequest {
    pub(super) mode: punktfunk_core::Mode,
    /// The wire budget; the encoder opens at what `enc_of` derives from it.
    pub(super) bitrate_kbps: u32,
    pub(super) bitrate_auto: bool,
    pub(super) bit_depth: u8,
    pub(super) enc_of: EncDerive,
    pub(super) plan: crate::session_plan::SessionPlan,
    /// The session's `PUNKTFUNK_PYROWAVE_MAX_MBPS` cap, kbps, for a pin re-resolved at the
    /// delivered size.
    pub(super) pyrowave_max_kbps: Option<u32>,
    /// The session's encode resolution: capture and the encoder open on it, whatever the
    /// Encoder row says now.
    pub(super) encoder: Arc<crate::encode::Resolved>,
    pub(super) client_hdr: Option<pf_frame::HdrMeta>,
    /// The loop's `au_seq`: a driver encoder's wire index continues there.
    pub(super) wire_seq_base: u32,
    /// The session's screen this build shows, `0` the first ([`super::screens`]).
    pub(super) screen: u8,
}

/// The session a Welcome-time [`prepare_display`] builds for, as the handshake settled it.
pub(in crate::native) struct DisplayPrep {
    pub(in crate::native) compositor: crate::vdisplay::Compositor,
    pub(in crate::native) mode: punktfunk_core::Mode,
    pub(in crate::native) client_identity: Option<[u8; 32]>,
    pub(in crate::native) client_hdr: Option<pf_frame::HdrMeta>,
    pub(in crate::native) cursor_forward: bool,
    pub(in crate::native) slices: u32,
    pub(in crate::native) bitrate_kbps: u32,
    pub(in crate::native) bitrate_auto: bool,
    pub(in crate::native) bit_depth: u8,
    pub(in crate::native) hdr: bool,
    /// The colours the client asked to get at studio range
    /// ([`crate::session_plan::SessionPlan::studio_asked`]).
    pub(in crate::native) studio_asked: crate::session_plan::StudioAsk,
    pub(in crate::native) enc_of: EncDerive,
    pub(in crate::native) chroma: crate::encode::ChromaFormat,
    pub(in crate::native) codec: crate::encode::Codec,
    pub(in crate::native) pyrowave_bpp: f64,
    pub(in crate::native) pyrowave_max_kbps: Option<u32>,
    pub(in crate::native) shard_payload: u16,
    /// A joiner's view and fit; `Some` for a session `mode_conflict: join` admitted.
    pub(in crate::native) reframe_to: Option<(punktfunk_core::video_fit::VideoFit, (u32, u32))>,
    /// The handshake's encode resolution, which the session keeps.
    pub(in crate::native) encoder: Arc<crate::encode::Resolved>,
}

/// A virtual-display session's plan on `compositor`, `pyrowave_bpp` and `encoder` the
/// session's, at the range `studio_asked` leads to. Bring-up, the Welcome-time prep and a codec
/// change all resolve here, so a prepared pipeline is built from the plan its session runs.
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_plan(
    encoder: &crate::encode::Resolved,
    bit_depth: u8,
    hdr: bool,
    chroma: crate::encode::ChromaFormat,
    codec: crate::encode::Codec,
    studio_asked: crate::session_plan::StudioAsk,
    cursor_forward: bool,
    slices: u32,
    compositor: crate::vdisplay::Compositor,
    route: Option<&crate::vdisplay::GamescopeRoute>,
    pyrowave_bpp: f64,
    shard_payload: usize,
    reframe_to: Option<(punktfunk_core::video_fit::VideoFit, (u32, u32))>,
    join_live: bool,
) -> crate::session_plan::SessionPlan {
    let mut plan = crate::session_plan::SessionPlan::resolve(
        encoder,
        bit_depth,
        hdr,
        chroma,
        codec,
        crate::session_plan::cursor_blend_for(
            cursor_forward,
            compositor,
            codec,
            bit_depth,
            hdr,
            route,
            encoder,
        ),
        cursor_forward,
        slices,
    );
    // After resolve: a self-painting gamescope node would otherwise get a second XFixes pointer.
    plan.gamescope_cursor = crate::session_plan::gamescope_cursor_for(
        compositor == pf_vdisplay::Compositor::Gamescope,
        route,
    );
    plan.sdr10_native = crate::session_plan::sdr10_native_for(&plan, compositor, route);
    plan.studio_asked = studio_asked;
    plan.pyrowave_bpp = pyrowave_bpp;
    if codec == crate::encode::Codec::PyroWave {
        plan.wire_chunk = Some(shard_payload);
    }
    plan.reframe_to = reframe_to;
    if join_live {
        plan.sharing_live_display()
    } else {
        plan
    }
}

/// Build display + pipeline at Welcome time, with the plan and display request bring-up would
/// use. Windows-only by policy (`handshake.rs` never spawns it elsewhere), not by construction.
pub(in crate::native) fn prepare_display(
    prep: DisplayPrep,
    quit: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    trace: &crate::bringup::Trace,
) -> Result<PreparedDisplay> {
    let DisplayPrep {
        compositor,
        mode,
        client_identity,
        client_hdr,
        cursor_forward,
        slices,
        bitrate_kbps,
        bitrate_auto,
        bit_depth,
        hdr,
        studio_asked,
        enc_of,
        chroma,
        codec,
        pyrowave_bpp,
        pyrowave_max_kbps,
        shard_payload,
        reframe_to,
        encoder,
    } = prep;
    let join_live = reframe_to.is_some();
    let plan = resolve_plan(
        &encoder,
        bit_depth,
        hdr,
        chroma,
        codec,
        studio_asked,
        cursor_forward,
        slices,
        compositor,
        None,
        pyrowave_bpp,
        shard_payload as usize,
        reframe_to,
        join_live,
    );
    let (_, metadata_composite) = super::cursor::composite_plan(
        &plan,
        cursor_forward,
        compositor == pf_vdisplay::Compositor::Gamescope,
    );
    let mut vd = crate::vdisplay::open(compositor)?;
    crate::vdisplay::SessionParams {
        client_fp: client_identity,
        client_hdr,
        hdr,
        hw_cursor: cursor_forward || metadata_composite,
        join_live,
        quit: quit.clone(),
        // Only Linux nests a launch in `create`, and Linux never preps.
        launch: None,
        route: None,
        isolation: None,
    }
    .apply(&mut *vd);
    let _idd_setup_guard = crate::windows::idd::setup_guard(
        plan.capture,
        client_identity,
        (mode.width, mode.height),
        stop,
    )?;
    let req = BuildRequest {
        mode,
        bitrate_kbps,
        bitrate_auto,
        bit_depth,
        enc_of,
        plan,
        pyrowave_max_kbps,
        encoder,
        client_hdr,
        wire_seq_base: 0,
        screen: 0,
    };
    let pipeline = build_pipeline_with_retry(&mut vd, &req, quit, stop, None, 1..=8, Some(trace))?;
    Ok(PreparedDisplay { vd, pipeline })
}

/// Run [`prepare_display`] on the thread that becomes the stream thread once Start delivers its
/// [`SessionContext`]. A dropped sender (handshake abort, a data plane that failed to bind) drops what was built,
/// which hands the monitor to keep-alive like a normal session end. `scope_done` is the
/// session scope's [`prep_done`](crate::native::scope::SessionScope::prep_done): the thread
/// sends on it once the stream takes it over and holds it to its last statement, so the
/// session's teardown waits for a prep it never took over. `None` when the thread does not
/// spawn: bring-up then builds inline. Compiled on every host; only Windows calls it.
pub(in crate::native) fn spawn_display_prep(
    prep: DisplayPrep,
    quit: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    trace: Arc<crate::bringup::Trace>,
    scope_done: std::sync::mpsc::SyncSender<()>,
) -> Option<PrepHandle> {
    let (ctx_tx, ctx_rx) = std::sync::mpsc::sync_channel::<SessionContext>(1);
    std::thread::Builder::new()
        .name("punktfunk1-stream".into())
        .spawn(move || -> Result<()> {
            // Declared first, so it drops last: after the display this thread built.
            let scope_done = scope_done;
            let done = AtomicBool::new(false);
            std::thread::scope(|s| {
                // Start lands while the display builds: its plane serves the client's ramp
                // meanwhile. A failed spawn drops `ctx_rx`, and the caller builds inline.
                let (done, scope_done) = (&done, &scope_done);
                let ramp = std::thread::Builder::new()
                    .name("punktfunk-ramp".into())
                    .spawn_scoped(s, move || {
                        let mut ctx = ctx_rx.recv().ok()?;
                        let _ = scope_done.try_send(());
                        super::ramp::serve_while_prepping(&mut ctx.common, done);
                        Some(ctx)
                    })?;
                let prepared = prepare_display(prep, &quit, &stop, &trace);
                done.store(true, Ordering::SeqCst);
                let ctx = ramp
                    .join()
                    .map_err(|_| anyhow::anyhow!("the bring-up ramp thread panicked"))?;
                match ctx {
                    Some(ctx) => virtual_stream(ctx, Some(prepared?)),
                    None => Ok(()),
                }
            })
        })
        .map(|handle| (ctx_tx, handle))
        .map_err(|e| {
            tracing::warn!(error = %e,
                "display-prep thread spawn failed — falling back to inline bring-up")
        })
        .ok()
}

/// A first attempt's first-frame budget; later attempts wait the capturer's own default.
const FIRST_ATTEMPT_FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(2500);

/// Retry transient first-frame races. Permanent errors short-circuit, except a driver with no
/// render device, which gets one adapter reload first ([`cycled_driver_for`]). Each failed
/// attempt drops its capturer so the next create is clean. `supersedes` is the lease this
/// build replaces (create-before-drop): without it the registry counts the old lease as a live
/// sibling and the new display extends the group instead of heading it. `attempts` are the
/// attempt numbers to run: attempt 1 takes [`FIRST_ATTEMPT_FRAME_BUDGET`], every other the
/// capturer's default, and a later attempt checks `stop` before it starts.
pub(super) fn build_pipeline_with_retry(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    req: &BuildRequest,
    quit: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    supersedes: Option<u64>,
    attempts: std::ops::RangeInclusive<u32>,
    trace: Option<&crate::bringup::Trace>,
) -> Result<Pipeline> {
    // IDD-push: hold one lease across attempts so a failed capturer drop does not Lingering-preempt.
    let _retry_hold = if matches!(
        req.plan.capture,
        crate::session_plan::CaptureBackend::IddPush
    ) {
        Some(
            vd.create(display_mode_for(req.mode, vd.name()))
                .context("acquire virtual output for the session (retry-hold lease)")?,
        )
    } else {
        None
    };
    let mut backoff = std::time::Duration::from_millis(500);
    let last = *attempts.end();
    for attempt in attempts {
        if attempt > 1 && stop.load(Ordering::SeqCst) {
            anyhow::bail!(
                "session ended (client disconnected) during pipeline build — aborting retries \
                 after {} attempt(s)",
                attempt - 1
            );
        }
        let first_frame_budget = (attempt == 1).then_some(FIRST_ATTEMPT_FRAME_BUDGET);
        match build_pipeline(vd, req, quit, supersedes, first_frame_budget, trace) {
            Ok(pipe) => {
                if attempt > 1 {
                    tracing::info!(attempt, "pipeline up after retry");
                }
                return Ok(pipe);
            }
            Err(e) => {
                let chain = format!("{e:#}");
                let permanent = is_permanent_build_error(&chain) && !cycled_driver_for(&e);
                if permanent || attempt == last {
                    let why = if permanent {
                        "permanent"
                    } else {
                        "out of retries"
                    };
                    return Err(e).with_context(|| {
                        format!("pipeline build failed ({why}) after {attempt} attempt(s)")
                    });
                }
                tracing::warn!(
                    attempt,
                    max = last,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %chain,
                    "pipeline build failed — retrying"
                );
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(std::time::Duration::from_secs(2));
            }
        }
    }
    unreachable!("the final attempt returns inside the loop")
}

/// A driver that cannot create its render device fails every open until the adapter reloads
/// into a fresh WUDFHost, so that failure earns one reload and a retry. At most once a minute
/// host-wide: the rebuild loop calls this once per build.
#[cfg(target_os = "windows")]
fn cycled_driver_for(e: &anyhow::Error) -> bool {
    const COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    if !is_driver_no_device(e) {
        return false;
    }
    {
        let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_some_and(|t| t.elapsed() < COOLDOWN) {
            return false;
        }
        *last = Some(std::time::Instant::now());
    }
    match crate::vdisplay::driver::force_driver_cycle_if_sole() {
        Ok(()) => {
            tracing::warn!("driver adapter reloaded for a missing render device — retrying");
            true
        }
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "driver cycle for a missing render device did not run"
            );
            false
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn cycled_driver_for(_: &anyhow::Error) -> bool {
    false
}

/// `SET_ENCODE` answered `NO_DEVICE` for a render device that would not create. A missing
/// swap-chain is Windows' decision, which a reload does not change.
#[cfg(target_os = "windows")]
fn is_driver_no_device(e: &anyhow::Error) -> bool {
    e.downcast_ref::<pf_capture::DriverEncodeOpenError>()
        .is_some_and(|d| {
            d.status == pf_driver_proto::encode::SET_ENCODE_NO_DEVICE && !d.is_no_swap_chain()
        })
}

/// Permanent = retrying cannot help this session. Match our English prefix, not KWin's translated payload.
pub(super) fn is_permanent_build_error(chain: &str) -> bool {
    const PERMANENT: &[&str] = &[
        "virtual displays require linux",
        "unknown punktfunk_compositor",
        "compositor not detected",
        "kwin virtual output failed",
        "must be a node id",
        "is it installed",
        "capture/encoder negotiation mismatch",
        "driver encoder open failed",
        // A backend compiled out cannot appear on a retry, and each one costs
        // the user's real monitor a disable/restore cycle.
        "this build left out",
        // A pinned monitor the host does not have. Retrying holds the client on a
        // black screen for the whole backoff before saying so.
        "no monitor named",
        // Stock niri stays without virtual outputs until its owner installs biri.
        "this niri has no virtual outputs",
        // An output with no PipeWire node has nothing to fall back to.
        "has no pipewire node",
        // An Umbriel without virtual outputs stays so until it is updated.
        "predates virtual outputs",
    ];
    let lower = chain.to_ascii_lowercase();
    PERMANENT.iter().any(|p| lower.contains(p))
}

/// Session mode with refresh × the display multiple ([`HostConfig::vdisplay_hz_mult_for`]) for
/// the `compositor` that creates the display. Wire rate is still [`pacing_hz`]. DWM composes on
/// the display's own fixed tick, which the automatic multiple is for; KWin's unpaced cast records
/// each commit, so it gets none. gamescope paints on the game's commit, so a multiple would only
/// raise its frame limit: none there, set or not.
///
/// [`HostConfig::vdisplay_hz_mult_for`]: pf_host_config::HostConfig::vdisplay_hz_mult_for
pub(super) fn display_mode_for(
    session: punktfunk_core::Mode,
    compositor: &str,
) -> punktfunk_core::Mode {
    let mult = if compositor == pf_vdisplay::Compositor::Gamescope.id() {
        1
    } else {
        pf_host_config::config().vdisplay_hz_mult_for(session.refresh_hz, cfg!(windows))
    };
    punktfunk_core::Mode {
        refresh_hz: session.refresh_hz.saturating_mul(mult).min(0xffff),
        ..session
    }
}

/// The virtual display is gamescope's: it paints on the game's commit.
pub(super) fn is_gamescope(vd: &dyn crate::vdisplay::VirtualDisplay) -> bool {
    vd.name() == pf_vdisplay::Compositor::Gamescope.id()
}

/// Pace and encode at min(session, achieved). Overdrive is display-only.
pub(super) fn pacing_hz(session_hz: u32, achieved_hz: u32) -> u32 {
    achieved_hz.min(session_hz).max(1)
}

/// Log a display that runs below the rate the session asked. A mirrored head
/// ([`DisplayOwnership::External`]) was asked for none and runs at its own: that is a fact
/// about the head, not a compositor refusing a request.
fn report_achieved_hz(
    ownership: crate::vdisplay::DisplayOwnership,
    display_hz: u32,
    achieved_hz: u32,
    session_hz: u32,
) {
    if achieved_hz < session_hz {
        if ownership == crate::vdisplay::DisplayOwnership::External {
            tracing::info!(
                head = achieved_hz,
                session = session_hz,
                "the mirrored head runs below the session rate — encoding at the head's rate"
            );
        } else {
            tracing::warn!(
                requested = display_hz,
                achieved = achieved_hz,
                session = session_hz,
                "compositor did not honor the requested refresh — encoding at the achieved rate"
            );
        }
    } else if achieved_hz < display_hz {
        tracing::info!(
            requested = display_hz,
            achieved = achieved_hz,
            session = session_hz,
            "compositor did not honor the multiplied display refresh — the session rate is unaffected"
        );
    }
}

/// Open the session's encoder for `frame` on `encoder`, the session's resolution — framed for a
/// joiner, or at the client's `negotiated` size when a larger head is mirrored
/// (`session_plan::open_encoder_fitted`) — with the plan's chunking, slice ceiling and the
/// capturer's ring depth applied, and return the framing it encodes. `bitrate_bps` is asked for
/// that size; depth and HDR are what the source composes now (`session_plan::live_depth`). An
/// IDD-push source gets the driver's encoder instead, its wire-index domain continuing at
/// `wire_seq_base` (the loop's `au_seq`). Both stream loops open here: GameStream's plan carries
/// the client's own slice count.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_session_encoder(
    encoder: &crate::encode::Resolved,
    plan: &crate::session_plan::SessionPlan,
    capturer: &dyn crate::capture::Capturer,
    frame: &crate::capture::CapturedFrame,
    negotiated: (u32, u32),
    hz: u32,
    bitrate_bps: impl Fn(u32, u32) -> u64,
    bit_depth: u8,
    client_hdr: Option<pf_frame::HdrMeta>,
    wire_seq_base: u32,
) -> Result<(
    Box<dyn crate::encode::Encoder>,
    punktfunk_core::video_fit::Reframe,
)> {
    open_session_encoder_with(
        encoder,
        plan,
        capturer,
        frame,
        negotiated,
        hz,
        bitrate_bps,
        bit_depth,
        client_hdr,
        wire_seq_base,
        crate::encode::open_video,
    )
}

/// [`open_session_encoder`] through `open_video`, which a test replaces.
#[allow(clippy::too_many_arguments)]
pub(super) fn open_session_encoder_with(
    encoder: &crate::encode::Resolved,
    plan: &crate::session_plan::SessionPlan,
    capturer: &dyn crate::capture::Capturer,
    frame: &crate::capture::CapturedFrame,
    negotiated: (u32, u32),
    hz: u32,
    bitrate_bps: impl Fn(u32, u32) -> u64,
    bit_depth: u8,
    client_hdr: Option<pf_frame::HdrMeta>,
    wire_seq_base: u32,
    mut open_video: impl FnMut(
        &crate::encode::Resolved,
        crate::encode::OpenParams,
    ) -> Result<Box<dyn crate::encode::Encoder>>,
) -> Result<(
    Box<dyn crate::encode::Encoder>,
    punktfunk_core::video_fit::Reframe,
)> {
    if plan.capture.encodes_remotely() {
        return crate::windows::idd::open_driver_encoder(
            plan,
            capturer,
            (frame.width, frame.height),
            hz,
            bitrate_bps(frame.width, frame.height),
            bit_depth,
            client_hdr,
            wire_seq_base,
        )
        .map(|e| {
            (
                e,
                punktfunk_core::video_fit::Reframe::full((frame.width, frame.height)),
            )
        });
    }
    let (hdr, bit_depth) =
        crate::session_plan::live_depth(plan.hdr, capturer.colour().hdr_meta.is_some(), bit_depth);
    let (mut enc, framed) = crate::session_plan::open_encoder_fitted(
        frame,
        negotiated,
        plan.reframe_to,
        |width, height| {
            open_video(
                encoder,
                crate::encode::OpenParams {
                    codec: plan.codec,
                    format: frame.format,
                    width,
                    height,
                    fps: hz,
                    bitrate_bps: bitrate_bps(width, height),
                    cuda: frame.is_cuda(),
                    bit_depth,
                    hdr,
                    chroma: plan.chroma,
                    cursor_blend: plan.cursor_blend,
                    slices: plan.slices,
                    full_range: plan.full_range(hdr, bit_depth),
                },
            )
        },
    )?;
    if let Some(c) = plan.wire_chunk {
        enc.set_wire_chunking(c);
    }
    // The host submits one frame in flight; an NVENC never told so pipelines two.
    enc.set_input_ring_depth(1);
    Ok((enc, framed))
}

/// Acquire a display at `req`'s mode and attach capture and an encoder to it.
pub(super) fn build_pipeline(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    req: &BuildRequest,
    quit: &Arc<AtomicBool>,
    supersedes: Option<u64>,
    first_frame_budget: Option<std::time::Duration>,
    trace: Option<&crate::bringup::Trace>,
) -> Result<Pipeline> {
    let display_mode = display_mode_for(req.mode, vd.name());
    let vout = crate::vdisplay::registry::acquire(vd, display_mode, quit.clone(), supersedes)
        .context("create virtual output")?;
    if let Some(t) = trace {
        t.mark("display_acquired");
    }
    attach_pipeline(vd, vout, req, display_mode, first_frame_budget, trace)
}

/// A capture-only rebuild: the output the session already holds, a new capture and encoder
/// on it. `keepalive` is what the old capturer handed back; it moves into the new one, or
/// drops with the error and releases the output like any failed build.
#[cfg(target_os = "linux")]
pub(super) fn reattach_pipeline(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    lease: OutputLease,
    keepalive: Box<dyn Send>,
    req: &BuildRequest,
) -> Result<Pipeline> {
    let display_mode = display_mode_for(req.mode, vd.name());
    attach_pipeline(
        vd,
        lease.into_output(keepalive),
        req,
        display_mode,
        Some(FIRST_ATTEMPT_FRAME_BUDGET),
        None,
    )
}

/// Capture + encoder on an acquired output. Consumes `vout`: the capturer owns its keepalive.
fn attach_pipeline(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    vout: crate::vdisplay::VirtualOutput,
    req: &BuildRequest,
    display_mode: punktfunk_core::Mode,
    first_frame_budget: Option<std::time::Duration>,
    trace: Option<&crate::bringup::Trace>,
) -> Result<Pipeline> {
    let BuildRequest {
        mode,
        bitrate_kbps,
        bitrate_auto,
        bit_depth,
        enc_of,
        plan,
        pyrowave_max_kbps,
        ref encoder,
        client_hdr,
        wire_seq_base,
        screen,
    } = *req;
    #[cfg(target_os = "linux")]
    let reused_gen = vout.reused_gen;
    #[cfg(target_os = "linux")]
    let lease = OutputLease::of(&vout);
    #[cfg(target_os = "linux")]
    let pool_gen = vout.pool_gen;
    #[cfg(not(target_os = "linux"))]
    let pool_gen = None;
    let node_id = vout.node_id;
    let achieved_hz = vout
        .preferred_mode
        .map(|(_, _, hz)| hz)
        .filter(|&hz| hz > 0)
        .unwrap_or(display_mode.refresh_hz);
    report_achieved_hz(
        vout.ownership,
        display_mode.refresh_hz,
        achieved_hz,
        mode.refresh_hz,
    );
    let effective_hz = pacing_hz(mode.refresh_hz, achieved_hz);
    // A mirrored KWin head is still KWin's stream: its id 0 hides the pointer.
    let cursor_id0_hides = vd.producer() == pf_vdisplay::Compositor::Kwin.id();
    let producer_is_gamescope = is_gamescope(vd.as_ref());
    let mut capturer = crate::capture::capture_virtual_output(
        vout,
        crate::capture::VirtualCaptureRequest {
            output: plan.output_format(encoder),
            codec: Some(plan.codec),
            encoder: encoder.clone(),
            capture: plan.capture,
            kwin: cursor_id0_hides,
            gamescope: producer_is_gamescope,
            stream_hz: effective_hz,
            screen,
            gamescope_cursor: plan.gamescope_cursor,
        },
    )
    .context("capture virtual output")?;
    if let Some(t) = trace {
        t.mark("capture_attached");
    }
    if let Some(feed) = capturer.feed() {
        feed.set_active(true);
    }
    let first = capturer.next_frame(first_frame_budget.map_or(
        pf_capture::FirstFrame::Default,
        pf_capture::FirstFrame::Provisional,
    ));
    let frame = match first.context("first frame") {
        Ok(f) => f,
        Err(e) => {
            #[cfg(target_os = "linux")]
            if let Some(g) = reused_gen {
                crate::vdisplay::registry::mark_failed(g);
            }
            return Err(e);
        }
    };
    if let Some(t) = trace {
        t.mark("first_frame");
    }
    let negotiated = (mode.width, mode.height);
    // Automatic bitrate and every PyroWave pin follow the pixels actually encoded: a
    // mirrored head's fit, or whatever a virtual display delivered.
    let kbps_for = |w: u32, h: u32| {
        let encoded = punktfunk_core::Mode {
            width: w,
            height: h,
            ..mode
        };
        if (w, h) == negotiated {
            bitrate_kbps
        } else if plan.codec == crate::encode::Codec::PyroWave {
            let running = bitrate_auto.then_some(bitrate_kbps);
            pyrowave_mode_kbps(running, &encoded, &plan, pyrowave_max_kbps)
        } else if bitrate_auto {
            resolve_bitrate_kbps_for(
                plan.codec,
                0,
                &encoded,
                plan.chroma,
                bit_depth,
                plan.pyrowave_bpp,
                pyrowave_max_kbps,
            )
        } else {
            bitrate_kbps
        }
    };
    let (enc, reframe) = open_session_encoder(
        encoder,
        &plan,
        &*capturer,
        &frame,
        negotiated,
        effective_hz,
        |w, h| enc_of.enc_kbps(kbps_for(w, h)) as u64 * 1000,
        bit_depth,
        client_hdr,
        wire_seq_base,
    )
    .context("open video encoder")?;
    let encoded = reframe.out;
    let re = kbps_for(encoded.0, encoded.1);
    if re != bitrate_kbps {
        tracing::info!(
            negotiated = %format!("{}x{}", mode.width, mode.height),
            encoded = %format!("{}x{}", encoded.0, encoded.1),
            from_kbps = bitrate_kbps,
            to_kbps = re,
            "the encoder opened at a size other than the session negotiated — re-resolved the \
             Automatic bitrate for the pixels actually being encoded"
        );
    }
    let bitrate_kbps = re;
    if let Some(t) = trace {
        t.mark("encoder_open");
    }
    let opened_444 = enc.caps().chroma_444;
    if opened_444 != plan.chroma.is_444() {
        tracing::warn!(
            negotiated_444 = plan.chroma.is_444(),
            opened_444,
            "encoder chroma disagrees with the negotiated Welcome — the client was told the other value"
        );
    }
    let interval = std::time::Duration::from_secs_f64(1.0 / effective_hz.max(1) as f64);
    Ok(Pipeline {
        capturer,
        enc,
        frame,
        interval,
        node_id,
        display_gen: pool_gen,
        bitrate_kbps,
        reframe,
        #[cfg(target_os = "linux")]
        lease,
        #[cfg(target_os = "linux")]
        panel_tick_ns: panel_tick_ns(achieved_hz, effective_hz),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Capturer;

    /// Encodes nothing.
    struct NullEnc;

    impl crate::encode::Encoder for NullEnc {
        fn submit(&mut self, _frame: &crate::capture::CapturedFrame) -> Result<()> {
            Ok(())
        }
        fn poll(&mut self) -> Result<Option<crate::encode::EncodedFrame>> {
            Ok(None)
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn plan(
        chroma: crate::encode::ChromaFormat,
        hdr: bool,
        slices: u32,
    ) -> crate::session_plan::SessionPlan {
        crate::session_plan::SessionPlan {
            capture: crate::session_plan::CaptureBackend::Portal,
            topology: crate::session_plan::SessionTopology::SingleProcess,
            encoder: crate::session_plan::EncoderBackend::Software,
            bit_depth: if hdr { 10 } else { 8 },
            hdr,
            chroma,
            codec: crate::encode::Codec::H265,
            wire_chunk: None,
            pyrowave_bpp: 0.0,
            cursor_blend: true,
            cursor_forward: false,
            gamescope_cursor: false,
            sdr10_native: false,
            slices,
            reframe_to: None,
            rgb_full_range: true,
            studio_asked: crate::session_plan::StudioAsk::default(),
        }
    }

    /// Native opens a 4:4:4 HDR session at its own depth and a rate per encoded size;
    /// GameStream a 4:2:0 one at the client's slice count, the frame's depth and one rate.
    /// Over a source gone SDR the HDR session opens 8-bit SDR.
    #[test]
    fn both_loops_open_the_encoder_their_plan_names() {
        let frame = crate::capture::SyntheticCapturer::new(64, 32, 60)
            .next_frame(pf_capture::FirstFrame::Default)
            .expect("a synthetic frame");
        // A source lit HDR.
        let cap = pf_capture::FakeCapturer {
            colour: pf_capture::ColourFacts {
                hdr_meta: Some(pf_frame::hdr::generic_hdr10()),
                sdr_fp16: false,
            },
            ..Default::default()
        };
        let want = |plan: &crate::session_plan::SessionPlan, hz, bitrate_bps, bit_depth| {
            crate::encode::OpenParams {
                codec: plan.codec,
                format: frame.format,
                width: 64,
                height: 32,
                fps: hz,
                bitrate_bps,
                cuda: false,
                bit_depth,
                hdr: plan.hdr,
                chroma: plan.chroma,
                cursor_blend: true,
                slices: plan.slices,
                full_range: plan.full_range(plan.hdr, bit_depth),
            }
        };
        let encoder = crate::encode::resolve();
        let open = |plan: &crate::session_plan::SessionPlan,
                    hz,
                    bitrate_bps: fn(u32, u32) -> u64,
                    bit_depth| {
            let mut asked = Vec::new();
            let (_, reframe) = open_session_encoder_with(
                &encoder,
                plan,
                &cap,
                &frame,
                (64, 32),
                hz,
                bitrate_bps,
                bit_depth,
                None,
                1,
                |_, v| {
                    asked.push(v);
                    Ok(Box::new(NullEnc))
                },
            )
            .expect("an encoder");
            assert!(reframe.is_full());
            asked
        };
        let native = plan(
            crate::encode::ChromaFormat::Yuv444,
            true,
            crate::session_plan::DEFAULT_SLICES,
        );
        let asked = open(&native, 120, |w, h| u64::from(w * h), 10);
        assert_eq!(asked, [want(&native, 120, 64 * 32, 10)]);
        let gamestream = plan(crate::encode::ChromaFormat::Yuv420, false, 4);
        let asked = open(&gamestream, 60, |_, _| 16_287_878, 8);
        assert_eq!(asked, [want(&gamestream, 60, 16_287_878, 8)]);

        let sdr = crate::capture::SyntheticCapturer::new(64, 32, 60);
        let (_, _) = open_session_encoder_with(
            &encoder,
            &native,
            &sdr,
            &frame,
            (64, 32),
            120,
            |w, h| u64::from(w * h),
            10,
            None,
            1,
            |_, v| {
                assert_eq!((v.hdr, v.bit_depth), (false, 8), "an SDR source");
                Ok(Box::new(NullEnc))
            },
        )
        .expect("an encoder");
    }

    #[test]
    fn pacing_never_exceeds_the_session_rate_or_the_display() {
        assert_eq!(pacing_hz(120, 120), 120);
        assert_eq!(pacing_hz(120, 60), 60);
        assert_eq!(pacing_hz(60, 120), 60);
        assert_eq!(pacing_hz(120, 240), 120);
        assert_eq!(pacing_hz(60, 90), 60);
        assert_eq!(pacing_hz(60, 0), 1);
    }

    #[test]
    fn display_mode_multiplier_scales_only_the_refresh() {
        let session = punktfunk_core::Mode {
            width: 2560,
            height: 1440,
            refresh_hz: 60,
        };
        let cfg = pf_host_config::config();
        let display = display_mode_for(session, "kwin");
        assert_eq!((display.width, display.height), (2560, 1440));
        assert_eq!(
            display.refresh_hz,
            60 * cfg.vdisplay_hz_mult_for(60, cfg!(windows))
        );
        assert_eq!(
            display_mode_for(session, "pf-vdisplay").refresh_hz,
            60 * cfg.vdisplay_hz_mult_for(60, cfg!(windows))
        );
        assert_eq!(
            display_mode_for(session, "gamescope").refresh_hz,
            60,
            "gamescope paints on commit: no multiplier"
        );
    }

    #[test]
    fn permanent_errors_short_circuit_retry() {
        assert!(is_permanent_build_error(
            "create virtual output: KWin virtual output failed: Could not find output"
        ));
        assert!(is_permanent_build_error(
            "create virtual output: KWin virtual output failed: Não foi possível encontrar saída"
        ));
        assert!(is_permanent_build_error(
            "unknown PUNKTFUNK_COMPOSITOR 'foo' (kwin|wlroots|mutter|gamescope)"
        ));
        assert!(is_permanent_build_error(
            "spawn gamescope (is it installed? `apt install gamescope`)"
        ));
        assert!(is_permanent_build_error("virtual displays require Linux"));
        assert!(is_permanent_build_error(
            "open capture: direct capture refused and the backend has no PipeWire node: no \
             ext_image_copy_capture_manager_v1"
        ));
        assert!(is_permanent_build_error(
            "create virtual output: umbriel has no output-destroy: it predates virtual outputs \
             (2026-09-28); update it"
        ));
        assert!(!is_permanent_build_error(
            "create virtual output: KWin created the virtual output disabled and refused to \
             stream it (stream_virtual_output failed: Não foi possível encontrar saída); enabled \
             it over output management (head Virtual-punktfunk-a1b2) — the retry picks up the \
             configuration KWin just persisted"
        ));
        assert!(!is_permanent_build_error(
            "first frame: no PipeWire frame within 10s (node 42): format negotiation never completed"
        ));
        assert!(!is_permanent_build_error(
            "create virtual output: timed out creating the KWin virtual output"
        ));
        assert!(is_permanent_build_error(
            "open video encoder: hevc on NVIDIA needs the direct-SDK NVENC backend, which this \
             build left out — build with --features punktfunk-host/nvenc"
        ));
        assert!(is_permanent_build_error(
            "create virtual output: no monitor named \"HDMI-A-3\" — this host has: HDMI-A-1"
        ));
        assert!(!is_permanent_build_error("open NVENC: device busy"));
    }

    /// Stock niri does not grow virtual outputs between attempts. Reads the real error's chain.
    #[cfg(target_os = "linux")]
    #[test]
    fn stock_niri_is_permanent() {
        let chain = format!(
            "{:#}",
            anyhow::Error::new(pf_vdisplay::NoVirtualOutputs).context("create virtual output")
        );
        assert!(is_permanent_build_error(&chain), "{chain}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn driver_no_device_is_found_through_the_context_chain() {
        use pf_driver_proto::encode::{SET_ENCODE_NO_BACKEND, SET_ENCODE_NO_DEVICE};
        let open = |status, name: &str| {
            anyhow::Error::new(pf_capture::DriverEncodeOpenError {
                backends: [1, 0, 0, 0],
                status,
                error: 0x8007_000Eu32 as i32,
                name: name.into(),
            })
            .context("open video encoder")
        };
        assert!(is_driver_no_device(&open(SET_ENCODE_NO_DEVICE, "d3d11")));
        assert!(!is_driver_no_device(&open(SET_ENCODE_NO_BACKEND, "d3d11")));
        // A dark console display: Windows never assigned a swap-chain. No reload.
        assert!(!is_driver_no_device(&open(SET_ENCODE_NO_DEVICE, "noswap")));
    }
}
