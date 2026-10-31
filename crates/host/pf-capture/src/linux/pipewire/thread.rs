//! The PipeWire loop thread: connect, offer, negotiate, then run `.process` until quit.

use super::config::StreamConfig;
use super::consume::{consume_frame, realtime_minus_monotonic_ns};
use super::ledger::{pool_ask, PoolCensus, PoolLedger};
use super::offers::{
    hdr_modifier_offers, offer_pacing, packed_modifier_offers, probe_producer, DeviceModifiers,
    ProducerProbe,
};
use super::pacer::{wire_interval, Pacer, RawTimer, RequestListener, HEARTBEAT};
use super::plan::{
    consumer_kind, resolved_capture_arm, ImportState, NegotiationPlan, PassthroughFallbacks,
};
use super::{map_format, CaptureOpts, FrameQueue, UserData};
use crate::linux::pw_cursor::{update_cursor_meta, CursorState};
use crate::linux::pw_pods::{
    build_cursor_meta_param, build_damage_meta_param, build_default_format_obj,
    build_dmabuf_buffers, build_dmabuf_format, build_hdr_dmabuf_format, build_header_meta_param,
    build_mappable_buffers, build_sdr10_dmabuf_format, build_shm_only_buffers,
    build_sync_timeline_meta_param, serialize_pod, video_raw, Extent, Pacing, HDR_FORMAT_ORDER,
    SPA_VIDEO_TRANSFER_SMPTE2084,
};
use crate::linux::sync_timeline::{StreamBuffers, SyncDevice};
use crate::linux::{CursorSlot, Fixated, ImporterCell, StreamShared, StreamStatus};
use anyhow::{Context, Result};
use pf_zerocopy::Ladder;
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::Pod;
use std::cell::Cell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;

/// How often the loop looks for buffers that never arrived. A lost one costs the producer a
/// pool slot until then, not a frame.
const UNDELIVERED_SWEEP: std::time::Duration = std::time::Duration::from_millis(100);

/// What the capturer sends its loop thread.
pub(in crate::linux) enum PwCtl {
    /// Stop the loop; the thread unwinds.
    Quit,
    /// Raise a wake edge with no frame behind it: the consumer's arrival wait ends now.
    Poke,
}

/// The loop thread's ends of what it shares with the capturer: frames go out through `queue`
/// and `wake`, the stream's state through the three cells.
pub(super) struct LoopEnds {
    /// Arrivals awaiting their renders. A consumer that falls behind loses the oldest.
    pub(super) queue: FrameQueue,
    pub(super) wake: SyncSender<()>,
    pub(super) shared: StreamShared,
    pub(super) cursor_slot: CursorSlot,
    pub(super) importer: ImporterCell,
}

/// The PipeWire loop thread for one capture session: connect, probe the producer, resolve the
/// [`Offer`], build the params, listen, and run `.process` until `quit_rx` or disconnect. A
/// broken capture consumes no more frames. Teardown after `run()`: the importer guard, then
/// the pacer sources, the undelivered timer, the requeue receiver, the listener and the
/// stream. An early error drops the same locals in reverse, the guard after the stream.
pub(super) fn pipewire_thread(
    cfg: StreamConfig,
    fd: Option<OwnedFd>,
    ends: LoopEnds,
    quit_rx: pw::channel::Receiver<PwCtl>,
) -> Result<()> {
    crate::pwinit::ensure_init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let loop_ = mainloop.loop_();
    // Capturer `Drop` lands here on the loop thread and stops `run()` so the thread unwinds
    // instead of blocking to process exit. Hold the attachment for the loop's life. The
    // registry probe below also runs the loop; `quit_seen` keeps a quit during it terminal.
    let quit_seen = Rc::new(Cell::new(false));
    let _quit_attach = {
        let (quit_loop, seen) = (mainloop.clone(), quit_seen.clone());
        // This clone dies with the thread, like the one `.process` holds: the consumer
        // still sees the channel close when the loop ends.
        let poke_wake = ends.wake.clone();
        quit_rx.attach(loop_, move |ctl| on_ctl(ctl, &quit_loop, &seen, &poke_wake))
    };
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = connect_core(&context, fd)?;
    // Lazy driver (PipeWire ≥ 1.2.7 "headless server" scheduling): the producer paints only
    // in a graph cycle this stream starts, so the encode loop owns the tick and no second
    // clock beats against it. Only a producer that emits RequestProcess (Mutter ≥ 49 virtual
    // monitors) may be driven this way; any other stays the driver as before.
    let probe = probe_producer(&core, &mainloop, cfg.node_id);
    let lazy = cfg.opts.lazy && probe.supports_request;
    if quit_seen.get() {
        return Ok(());
    }

    let mut importer = open_importer(&cfg.plan, cfg.opts.full_range);
    let ladder = ends.shared.health.ladder();
    let hdr_tiled_raw = cfg.hdr_tiled_raw(&ladder);
    let lists = DeviceModifiers::ask(importer.as_mut(), cfg.plan.nvenc_raw, hdr_tiled_raw);
    let offer = resolve_offer(&cfg, &ladder, &lists);
    // Latch must fire only for an offer actually made — `plan.build_importer` cannot know
    // the importer constructed.
    let gpu_offer = offer.want_dmabuf && !cfg.plan.vaapi_passthrough && !cfg.opts.want_hdr;
    ends.shared.offer_gpu(gpu_offer);
    log_resolved_arm(&cfg, &offer, &ladder);

    let (requeue_tx, requeue_rx) = pw::channel::channel::<()>();
    let sync = cfg
        .wants_sync(offer.want_dmabuf)
        .then(SyncDevice::open)
        .flatten()
        .map(Arc::new);
    let ledger = Arc::new(PoolLedger::new(requeue_tx, sync.clone()));
    // The heartbeat timer reads `driving` after `shared` moves into the listener's state.
    let shared_hb = ends.shared.clone();
    // A driven producer paints only in cycles this stream starts; the pacer starts one per
    // request, no sooner than a wire interval after the last. Every entry point runs on this
    // thread. The stream pointer lands once the stream exists.
    let pacer = lazy.then(|| Pacer::new(wire_interval(cfg.preferred, cfg.opts.stream_hz)));
    let importer_exit = ClearImporter::hand_over(importer, &ends.importer);
    let data = listener_state(
        &cfg,
        ends,
        ledger.clone(),
        pacer.clone(),
        sync.clone(),
        hdr_tiled_raw,
    );
    let stream = capture_stream(&core, lazy)?;
    if let Some(p) = &pacer {
        p.stream.set(stream.as_raw_ptr());
    }

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(on_state_changed)
        .param_changed(|_stream, ud, id, param| on_param_changed(ud, id, param))
        .add_buffer(|_stream, ud, buf| on_add_buffer(ud, buf as usize))
        .remove_buffer(|_stream, ud, buf| on_remove_buffer(ud, buf as usize))
        .process(|stream, ud| on_process(&stream.as_raw_ptr(), ud))
        .register()
        .context("register stream listener")?;
    let _requeue = install_ledger(loop_, &stream, &ledger, requeue_rx, sync.is_some());

    log_format_offer(&cfg.opts);
    let pods = build_params(&cfg, &probe, &offer, sync.is_some())?;
    connect_stream(&stream, cfg.node_id, lazy, &pods)?;
    let _pacing = pacer
        .as_ref()
        .map(|p| install_pacer(loop_, &stream, p, shared_hb));

    // Blocks until capturer `Drop` fires the quit channel.
    mainloop.run();
    drop(importer_exit);
    Ok(())
}

/// The capturer's control channel, on the loop thread.
fn on_ctl(
    ctl: PwCtl,
    mainloop: &pw::main_loop::MainLoopRc,
    quit_seen: &Cell<bool>,
    wake: &SyncSender<()>,
) {
    match ctl {
        PwCtl::Quit => {
            tracing::debug!("pipewire: quit signal received — stopping capture loop");
            quit_seen.set(true);
            mainloop.quit();
        }
        // A full channel already holds an edge; the consumer wakes either way.
        PwCtl::Poke => {
            let _ = wake.try_send(());
        }
    }
}

/// Portal source: fd to a sandboxed PipeWire remote. A virtual output: no fd, the live
/// session's daemon, by its socket when a session is stored.
fn connect_core(context: &pw::context::ContextRc, fd: Option<OwnedFd>) -> Result<pw::core::CoreRc> {
    match fd {
        Some(fd) => context
            .connect_fd_rc(fd, None)
            .context("pw connect_fd (portal remote)"),
        None => context
            .connect_rc(
                pf_host_config::session::pipewire_remote()
                    .map(|socket| properties! { *pw::keys::REMOTE_NAME => socket }),
            )
            .context("pw connect (default daemon)"),
    }
}

/// The capture stream. `lazy` makes it the graph's driver.
fn capture_stream(core: &pw::core::Core, lazy: bool) -> Result<pw::stream::StreamBox<'_>> {
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE     => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE     => "Screen",
        // Do not let the session manager re-target this stream: an orphaned auto-link to
        // a fresh Video/Source wedges that node and head-blocks the daemon work queue,
        // stalling all new link negotiation system-wide.
        "node.dont-reconnect"     => "true",
    };
    if lazy {
        // "2" outranks the producer's supports-request, so PipeWire picks this node as the
        // driver and the producer becomes a requesting follower.
        props.insert("node.supports-lazy", "2");
    }
    pw::stream::StreamBox::new(core, "punktfunk-screencast", props).context("pw Stream")
}

/// Connect to `node_id` with `pods`, as the driver when `lazy`.
fn connect_stream(
    stream: &pw::stream::Stream,
    node_id: u32,
    lazy: bool,
    pods: &[Vec<u8>],
) -> Result<()> {
    let mut params: Vec<&Pod> = pods
        .iter()
        .map(|b| Pod::from_bytes(b).context("pod from bytes"))
        .collect::<Result<_>>()?;
    let mut flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
    if lazy {
        flags |= pw::stream::StreamFlags::DRIVER;
    }
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            flags,
            &mut params,
        )
        .context("pw stream connect")
}

/// The listener's state for one stream: the ends it publishes through, and what the config and
/// the offer decided.
fn listener_state(
    cfg: &StreamConfig,
    ends: LoopEnds,
    ledger: Arc<PoolLedger>,
    pacer: Option<Rc<Pacer>>,
    sync: Option<Arc<SyncDevice>>,
    hdr_tiled_raw: bool,
) -> UserData {
    let (plan, opts) = (&cfg.plan, &cfg.opts);
    UserData {
        info: VideoInfoRaw::default(),
        format: None,
        modifier: 0,
        queue: ends.queue,
        wake: ends.wake,
        shared: ends.shared,
        cursor_slot: ends.cursor_slot,
        importer: ends.importer,
        vaapi_passthrough: plan.vaapi_passthrough,
        // Same predicate `hdr_modifier_offers` used for the NVENC raw lane.
        hdr_tiled_raw,
        import_policy: plan.import_policy.for_ten_bit_sdr(opts.ten_bit_sdr),
        import_state: ImportState::default(),
        dbg_log_n: 0,
        pts: crate::pts_provenance::PtsProvenance::new(),
        pts_reported: std::time::Instant::now(),
        rt_minus_mono_ns: realtime_minus_monotonic_ns(),
        hdr_pts_enabled: opts.knobs.header_pts,
        pool: PoolCensus::default(),
        passthrough_fallbacks: PassthroughFallbacks::default(),
        cursor: CursorState::new(opts.cursor_id0_hides),
        expect_dims: if opts.expect_exact_dims {
            cfg.preferred.map(|(w, h, _)| (w, h))
        } else {
            None
        },
        gate_skips: 0,
        gate_since: None,
        ledger,
        pacer,
        held_drops: 0,
        damage: DamageGate::new(wire_interval(cfg.preferred, opts.stream_hz)),
        undamaged: 0,
        sync,
    }
}

/// Takes the shared importer away when the loop thread is done with it: after the loop runs,
/// or on an early setup error. The importer goes here, not with the last `ImporterCell`
/// clone: the next pipeline must find the EGL/CUDA state gone.
struct ClearImporter(ImporterCell);

impl ClearImporter {
    /// Hand `importer` to the consumer, which imports held frames at its own tick; the guard
    /// takes it back.
    fn hand_over(importer: Option<pf_zerocopy::Importer>, cell: &ImporterCell) -> ClearImporter {
        cell.present.store(importer.is_some(), Ordering::Relaxed);
        cell.in_process.store(
            importer
                .as_ref()
                .is_some_and(pf_zerocopy::Importer::in_process),
            Ordering::Relaxed,
        );
        *cell.slot.lock().unwrap_or_else(|e| e.into_inner()) = importer;
        ClearImporter(cell.clone())
    }
}

impl Drop for ClearImporter {
    fn drop(&mut self) {
        self.0.present.store(false, Ordering::Relaxed);
        *self.0.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// What this pipeline offers the producer, resolved once from the config and what the GPU
/// answered.
struct Offer {
    /// The EGL→CUDA importer constructed.
    importer: bool,
    /// BGRx dmabuf modifiers.
    modifiers: Vec<u64>,
    /// BGRA dmabuf modifiers (xdph lists only BGRA on its dmabuf EnumFormat).
    modifiers_bgra: Vec<u64>,
    /// Per-format 10-bit modifiers, in [`HDR_FORMAT_ORDER`].
    hdr_modifiers: Vec<(VideoFormat, Vec<u64>)>,
    /// The PyroWave device's Vulkan list joined the packed offer.
    extend_pyrowave: bool,
    /// Ask for dmabufs at all.
    want_dmabuf: bool,
}

/// The importer where the plan wants one, its YUV converts at `full_range`, between the plan's
/// two lines: a latched GPU import and the producer-native planar offer. The isolated worker
/// (design/zerocopy-worker-isolation.md) turns a driver fault into a dead worker, not a dead
/// host; a construction failure is the CPU path.
fn open_importer(plan: &NegotiationPlan, full_range: bool) -> Option<pf_zerocopy::Importer> {
    if plan.gpu_import_latched {
        tracing::warn!(
            "zero-copy GPU import disabled for this capture identity (repeated lost imports or \
             a previous dmabuf negotiation timeout) — using CPU path"
        );
    }
    let importer = if plan.build_importer {
        match pf_zerocopy::Importer::new_for_capture(full_range) {
            Ok(i) => Some(i),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "zero-copy import unavailable — using CPU path");
                None
            }
        }
    } else {
        None
    };
    if plan.prefer_native_nv12 || plan.prefer_native_p010 {
        tracing::info!(
            container = if plan.prefer_native_p010 {
                "P010"
            } else {
                "NV12"
            },
            "zero-copy: offering the producer's own planar LINEAR DMA-BUF first (no host \
             RGB CSC; PUNKTFUNK_PIPEWIRE_NV12=0 restores the packed-RGB negotiation)"
        );
    }
    importer
}

/// The modifier lists the config, the identity's ladder and the GPU's answers allow, and
/// whether to ask for dmabufs at all. Pure: the importer is built and asked before.
fn resolve_offer(cfg: &StreamConfig, ladder: &Ladder, lists: &DeviceModifiers) -> Offer {
    // Per-fourcc offers: importer lists plus the encoder-proved gamescope seed
    // and the PyroWave Vulkan list, finalized by `dmabuf_modifiers_for_producer`.
    let (modifiers, modifiers_bgra, extend_pyrowave) = packed_modifier_offers(cfg, ladder, lists);
    let hdr_modifiers = hdr_modifier_offers(cfg, ladder, lists);
    if extend_pyrowave {
        tracing::info!(
            count = modifiers.len(),
            "zero-copy: advertising the PyroWave device's Vulkan-importable dmabuf modifiers"
        );
    }
    let want_dmabuf = cfg.plan.want_dmabuf(lists.importer, &modifiers);
    Offer {
        importer: lists.importer,
        modifiers,
        modifiers_bgra,
        hdr_modifiers,
        extend_pyrowave,
        want_dmabuf,
    }
}

/// One line for the resolved arm and its consumer, then the line that explains the arm. The
/// detail lines explain an arm; they do not state which one this session took. A convert line
/// names the range it writes.
fn log_resolved_arm(cfg: &StreamConfig, offer: &Offer, ladder: &Ladder) {
    let (plan, policy, opts) = (&cfg.plan, &cfg.policy, &cfg.opts);
    let (modifiers, want_dmabuf) = (&offer.modifiers, offer.want_dmabuf);
    let vaapi_passthrough = plan.vaapi_passthrough;
    let consumer = consumer_kind(
        policy.pyrowave_session,
        policy.backend_is_vaapi,
        policy.backend_is_gpu,
    );
    let arm = resolved_capture_arm(plan, offer.importer, want_dmabuf);
    tracing::info!(
        capture_arm = arm.as_str(),
        consumer = consumer.as_str(),
        modifier_count = if cfg.ten_bit() {
            offer
                .hdr_modifiers
                .iter()
                .map(|(_, m)| m.len())
                .max()
                .unwrap_or(0)
        } else {
            modifiers.len()
        },
        // Latch state belongs on the same line as the arm: `cpu` is either "never dmabuf"
        // or "a prior failure we are still living with" — only the second is a bug.
        raw_dmabuf_latch = ladder.raw_state(),
        "capture pipeline resolved: {} → {}",
        arm.as_str(),
        consumer.as_str()
    );
    if plan.force_shm {
        tracing::info!(
            "capture: PUNKTFUNK_FORCE_SHM — race-free SHM download path (no dmabuf, no zero-copy)"
        );
    } else if plan.raw_dmabuf_latched {
        tracing::warn!(
            "zero-copy raw-dmabuf passthrough disabled for this capture identity (repeated \
             encoder import failures or a negotiation timeout) — capturing CPU frames instead"
        );
    } else if !want_dmabuf && (plan.build_importer || plan.vaapi_passthrough) {
        tracing::warn!("zero-copy: no importable dmabuf modifiers — using CPU path");
    } else if vaapi_passthrough {
        // PyroWave remains raw passthrough when its tiled lists are empty: LINEAR is valid.
        tracing::info!(
            native_nv12_preferred = plan.prefer_native_nv12,
            native_p010_preferred = plan.prefer_native_p010,
            modifier_count = modifiers.len(),
            pyrowave_extended = offer.extend_pyrowave,
            "zero-copy: advertising DMA-BUF modifiers for direct encoder import (LINEAR \
             always; native NV12 first when enabled, packed RGB fallback)"
        );
    } else if want_dmabuf {
        tracing::info!(
            bgrx_count = modifiers.len(),
            bgra_count = offer.modifiers_bgra.len(),
            // Sample is truncated to 6, LINEAR pushed last — reading the sample as the whole
            // list makes a good offer look tiled-only.
            linear_offered = modifiers.contains(&0),
            sample = ?&modifiers[..modifiers.len().min(6)],
            "zero-copy: advertising EGL-importable dmabuf modifiers (BGRx + BGRA pods)"
        );
    } else if consumer.cpu_is_downgrade() {
        // No dmabuf advertised: this is the CPU path. `raw_dmabuf_latched` already caught a
        // latched downgrade. Warn for every GPU consumer; software wants CPU frames.
        // `consumer_kind` is per-session so a PyroWave session on an NVIDIA host still warns
        // (the host-global encoder pref would have called it NVENC and logged nothing).
        tracing::warn!(
            consumer = consumer.as_str(),
            "{} encode with the CPU capture path (per-frame de-pad + CSC + upload) — \
             zero-copy is off for this capture ({}); set PUNKTFUNK_ZEROCOPY=1 to restore the \
             dmabuf default",
            consumer.as_str(),
            // The knob's own grammar decided (`pf_zerocopy`); a set-but-on value is not the cause.
            if !pf_zerocopy::enabled() {
                "PUNKTFUNK_ZEROCOPY is off"
            } else if opts.want_hdr && !policy.hdr_cuda_ok {
                // `build_importer` drops HDR when the encoder cannot take packed 10-bit
                // CUDA. Naming the output format would send the reader to the wrong knob.
                "this HDR session's encoder cannot ingest a 10-bit CUDA payload, so the capture \
                 stays on CPU frames"
            } else {
                "this session's output format asked for CPU frames"
            }
        );
    }
    if want_dmabuf && !vaapi_passthrough && opts.want_444 {
        tracing::info!(
            full_range = opts.full_range,
            "4:4:4 zero-copy: tiled dmabufs convert to planar YUV444 (BT.709) on the GPU — \
             NVENC fed native full-chroma YUV, no CPU pixel path"
        );
    } else if want_dmabuf && !vaapi_passthrough && pf_zerocopy::nv12_enabled() {
        tracing::info!(
            full_range = opts.full_range,
            "PUNKTFUNK_NV12: tiled dmabufs convert to NV12 (BT.709) on the GPU — NVENC fed \
             native YUV (no internal RGB→YUV CSC)"
        );
    }
}

/// The two format offers worth a line before the pods go out: the bisect pod and HDR.
fn log_format_offer(opts: &CaptureOpts) {
    if let Some((fw, fh)) = opts.knobs.fixed_pod {
        tracing::info!(
            fw,
            fh,
            "pipewire: offering a fixed BGRx format pod (PUNKTFUNK_PW_FIXED_POD)"
        );
    }
    if opts.want_hdr {
        tracing::info!(
            "HDR capture: offering xBGR_210LE/xRGB_210LE DMA-BUF modifiers (LINEAR always) \
             with MANDATORY BT.2020 + SMPTE-2084 (PQ) colorimetry"
        );
    }
}

/// Every param the stream connects with, in order: the format pods, the Buffers pods (the
/// explicit-sync twin that demands the meta ahead of the plain one), then the metas. Reads
/// nothing but its arguments; `sync` is whether the syncobj device opened. The planar pods pin
/// the session's range.
///
/// Zero-copy offers only dmabuf formats with modifiers we can import (offering SHM makes the
/// compositor pick SHM). Modifiers go out as MANDATORY `ChoiceEnum::Enum`; this is not the
/// two-step DONT_FIXATE handshake (`ChoiceFlags` cannot express it). gamescope paints the Steam
/// overlay into this node only when the negotiated `gamescope_focus_appid` is 0, the default:
/// never advertise a non-zero one (the Remote-Play branch, which drops the overlay).
fn build_params(
    cfg: &StreamConfig,
    probe: &ProducerProbe,
    offer: &Offer,
    sync: bool,
) -> Result<Vec<Vec<u8>>> {
    let (plan, opts, preferred, mhz) = (&cfg.plan, &cfg.opts, cfg.preferred, probe.framerate_mhz);
    let range = opts.full_range;
    let format_pods = |unpaced: bool| -> Result<Vec<Vec<u8>>> {
        let pacing = offer_pacing(unpaced, mhz, opts.producer_is_gamescope, preferred);
        if opts.want_hdr {
            // Offering SDR alongside lets the producer pick it, and a timeout latches SDR
            // downgrade. Order is the fix — see the NVIDIA note on `HDR_FORMAT_ORDER`. First
            // compatible pod wins, so gamescope's P010 pass leads when the encoder takes it.
            let mut pods = Vec::with_capacity(HDR_FORMAT_ORDER.len() + 1);
            if plan.prefer_native_p010 {
                pods.push(build_hdr_dmabuf_format(
                    VideoFormat::P010_10LE,
                    &[0],
                    preferred,
                    pacing,
                    range,
                )?);
            }
            for (fmt, list) in &offer.hdr_modifiers {
                pods.push(build_hdr_dmabuf_format(
                    *fmt, list, preferred, pacing, range,
                )?);
            }
            return Ok(pods);
        }
        if !offer.want_dmabuf {
            // The fixed bisect pod stays exactly what the operator typed.
            let o = match opts.knobs.fixed_pod {
                Some((fw, fh)) => video_raw(
                    pw::spa::pod::property!(
                        pw::spa::param::format::FormatProperties::VideoFormat,
                        Id,
                        VideoFormat::BGRx
                    ),
                    Extent::Fixed(fw, fh),
                    Pacing::Producer,
                ),
                None if unpaced => build_default_format_obj(preferred, Pacing::Unpaced),
                None => build_default_format_obj(preferred, Pacing::Producer),
            };
            return Ok(vec![serialize_pod(o)?]);
        }
        let mut pods = Vec::with_capacity(if plan.prefer_native_nv12 { 3 } else { 2 });
        if opts.sdr10_native {
            // gamescope's own 10-bit SDR, in the HDR set's order: P010 leads when the encoder
            // takes it, then packed 10-bit. The 8-bit pods stay behind as the fallback, so a
            // gamescope whose 10-bit pods are PQ-only still links.
            if plan.prefer_native_p010 {
                pods.push(build_sdr10_dmabuf_format(
                    VideoFormat::P010_10LE,
                    &[0],
                    preferred,
                    pacing,
                    range,
                )?);
            }
            for (fmt, list) in &offer.hdr_modifiers {
                pods.push(build_sdr10_dmabuf_format(
                    *fmt, list, preferred, pacing, range,
                )?);
            }
        }
        if plan.prefer_native_nv12 {
            // First compatible consumer pod wins. Pinning BT.709 at the session's range selects
            // gamescope's RGB→NV12 shader with our bitstream colorimetry.
            pods.push(build_dmabuf_format(
                VideoFormat::NV12,
                &[0],
                preferred,
                pacing,
                range,
            )?);
        }
        if !offer.modifiers.is_empty() {
            pods.push(build_dmabuf_format(
                VideoFormat::BGRx,
                &offer.modifiers,
                preferred,
                pacing,
                range,
            )?);
        }
        // xdph (Hyprland/sway) lists only BGRA on its dmabuf EnumFormat (BGRA+BGRx on SHM).
        // A BGRx-only dmabuf offer intersects nothing and the link fails as if modifiers
        // mismatched. Same 32-bit layout; listed after BGRx so a producer offering both
        // still takes the existing path (first compatible consumer pod wins).
        if !offer.modifiers_bgra.is_empty() {
            pods.push(build_dmabuf_format(
                VideoFormat::BGRA,
                &offer.modifiers_bgra,
                preferred,
                pacing,
                range,
            )?);
        }
        Ok(pods)
    };
    // Unpaced pods first, the plain set behind them. A KWin before 6.7 floors `maxFramerate`
    // at 1/1, so a fixed 0/1 fails every intersection and the plain set is what fixates.
    let mut params = format_pods(opts.unpaced)?;
    if opts.unpaced {
        params.extend(format_pods(false)?);
    }
    let pool_min = pool_ask(
        opts.pool_min,
        opts.pool_max,
        plan.nvenc_raw || plan.vaapi_passthrough,
    );
    // Explicit sync: a Buffers twin that demands the meta, ahead of the plain one, and the
    // meta itself. Both sides listing the meta is what puts the two syncobj datas on a buffer.
    if sync {
        params.push(build_dmabuf_buffers(pool_min, true)?);
    }
    params.push(if opts.want_hdr || offer.want_dmabuf {
        // Dmabuf-only. HDR: Mutter's SHM path paints 8-bit ARGB32 regardless of format, so a
        // MemFd buffer under a 10-bit format would carry mislabeled bytes.
        build_dmabuf_buffers(pool_min, false)?
    } else if plan.force_shm {
        // Exclude DmaBuf so Mutter must download (glReadPixels orders against render).
        build_shm_only_buffers()?
    } else {
        // CPU path still accepts mappable dmabufs (gamescope offers only those once its
        // modifier-bearing format pod wins).
        build_mappable_buffers()?
    });
    // gamescope sends no cursor meta. Node ids and remote fds do not identify a compositor
    // (Mutter and gamescope can both use the default daemon), so the contract is explicit.
    let cursor_meta = !opts.producer_is_gamescope;
    if cursor_meta {
        params.push(build_cursor_meta_param()?);
    }
    if sync {
        params.push(build_sync_timeline_meta_param()?);
    }
    // Any meta listed here narrows the producer's set to the intersection, so the header
    // and the damage ride along with the first one; a producer left unlisted keeps its
    // whole set.
    if cursor_meta || sync {
        params.push(build_header_meta_param()?);
        params.push(build_damage_meta_param()?);
    }
    Ok(params)
}

fn on_state_changed(
    stream: &pw::stream::Stream,
    ud: &mut UserData,
    old: pw::stream::StreamState,
    new: pw::stream::StreamState,
) {
    let streaming = matches!(new, pw::stream::StreamState::Streaming);
    // Valid only while Streaming. True = the pacer's triggers start every cycle;
    // false = the producer kept the tick.
    let driving = streaming && stream.is_driving();
    tracing::info!(?old, ?new, driving, "pipewire stream state");
    // `Streaming` with no buffers is a static desktop. Anything else means the source
    // went away; `try_latest` turns a sustained non-Streaming state into capture-loss
    // so the encode loop rebuilds instead of freezing on the last frame.
    ud.shared.report(match new {
        pw::stream::StreamState::Streaming => StreamStatus::Streaming { driving },
        pw::stream::StreamState::Error(_) => StreamStatus::Errored,
        _ => StreamStatus::Paused,
    });
    if let Some(p) = &ud.pacer {
        p.on_streaming(driving);
    }
}

/// `add_buffer`: the pool census, published to the consumer, and the buffer's ledger entry.
fn on_add_buffer(ud: &mut UserData, buf: usize) {
    ud.pool.add();
    ud.shared.pool.depth.store(ud.pool.live, Ordering::Relaxed);
    ud.ledger.add(buf);
}

/// `remove_buffer`: the buffer is freed under any hold, so the ledger forgets it and that
/// hold's later release is a no-op. A hold's generation covers the address naming a buffer
/// of the next pool. The census reaches the consumer as on `add_buffer`.
fn on_remove_buffer(ud: &mut UserData, buf: usize) {
    ud.pool.remove();
    ud.shared.pool.depth.store(ud.pool.live, Ordering::Relaxed);
    ud.ledger.remove(buf);
}

/// `param_changed`: a fixated Format pod sets the size, format and modifier every later
/// buffer is read by, and drops the imports cached for the pool it replaces. Each pod is
/// read on its own: what it leaves out is the default, not the format before.
fn on_param_changed(ud: &mut UserData, id: u32, param: Option<&Pod>) {
    let Some(param) = param else { return };
    if id != pw::spa::param::ParamType::Format.as_raw() {
        return;
    }
    let Ok((media_type, media_subtype)) = pw::spa::param::format_utils::parse_format(param) else {
        return;
    };
    if media_type != pw::spa::param::format::MediaType::Video
        || media_subtype != pw::spa::param::format::MediaSubtype::Raw
    {
        return;
    }
    // A fresh struct per pod: the parser writes only the properties a pod carries, so one
    // without `modifier` or `transferFunction` must not inherit the last format's. On `Err`
    // the format before stays and nothing is fixated: a malformed first pod times out as
    // "no accepted format", not as a format mismatch.
    let mut info = VideoInfoRaw::default();
    if let Err(e) = info.parse(param) {
        tracing::error!(
            error = %e,
            "pipewire: the negotiated Format pod does not parse — capture will time out \
             with no usable format"
        );
        return;
    }
    ud.info = info;
    // Renegotiation replaces the pool: cached per-buffer imports key on buffers
    // that no longer exist, and a recycled fd/inode must not resolve to a stale import.
    if let Some(imp) = ud
        .importer
        .slot
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        imp.clear_cache();
    }
    let sz = ud.info.size();
    ud.format = map_format(ud.info.format());
    ud.modifier = ud.info.modifier();
    // A 10-bit format is HDR only when the producer fixated PQ on it: gamescope offers its
    // 10-bit formats under BT.709 too (`sdr10_native`).
    let hdr = ud.format.is_some_and(|f| f.is_ten_bit())
        && ud.info.transfer_function() == SPA_VIDEO_TRANSFER_SMPTE2084;
    ud.shared.fixate(Fixated {
        hdr,
        size: (sz.width, sz.height),
    });
    tracing::info!(
        width = sz.width,
        height = sz.height,
        spa_format = ?ud.info.format(),
        mapped = ?ud.format,
        modifier = ud.modifier,
        hdr,
        transfer_function = ud.info.transfer_function(),
        color_primaries = ud.info.color_primaries(),
        "pipewire format negotiated"
    );
    if ud.format.is_none() {
        tracing::error!(
            spa_format = ?ud.info.format(),
            "negotiated a pixel format the encoder cannot consume — frames will be skipped"
        );
    }
}

/// `.process`, over the buffers of the stream that runs it: latest frame only. The newest
/// buffer passes the birth gate, then a stale-frame skip, then [`consume_frame`]. It is
/// requeued exactly once, unless a hold took it: then the hold owns the requeue
/// ([`PoolLedger::done`]), and doing both hands the producer the buffer twice. Dequeue and
/// requeue stay outside `catch_unwind`, where a panic would strand the buffer and shrink the
/// fixed pool.
fn on_process(stream: &dyn StreamBuffers, ud: &mut UserData) {
    let Some((newest, drained)) = drain_to_newest(stream, ud) else {
        return;
    };
    // Producer's actual pool depth, once per distinct value. `build_dmabuf_buffers`
    // asks for a range; the producer picks. Depth is the deferred-requeue budget:
    // ≤ SHALLOW_POOL cannot defer, and a requeued buffer may be rewritten mid-encode.
    if let Some(depth) = ud.pool.note_frame() {
        tracing::info!(
            pool_depth = depth,
            high_water = ud.pool.high_water,
            drained,
            "pipewire buffer pool negotiated — the producer's ACTUAL count \
             (add_buffer/remove_buffer): the deferred-requeue budget, and the rewrite \
             window for any frame published without a hold"
        );
    }
    if !birth_gate(ud) {
        // SAFETY: `newest` was dequeued from this stream and not yet requeued;
        // requeued exactly once here, then never touched (mirrors the null path).
        unsafe { ud.requeue_unpublished(stream, newest) };
        return;
    }
    // PipeWire dispatches from a C trampoline with no catch_unwind; a panic across that
    // FFI aborts the host. Contain inspect/consume — the only Rust here that can panic.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `newest` is the non-null buffer we still own (dequeued, not requeued);
        // `.buffer` is a `*mut spa_buffer` field libpipewire populated. This is a single field
        // load through a valid pointer — no mutation or aliasing.
        let spa_buf = unsafe { (*newest).buffer };

        // Cursor meta before the stale-frame skip: Mutter pointer-only moves arrive as
        // metadata-only CORRUPTED buffers we drop for pixels, but the cursor is fresh.
        update_cursor_meta(&mut ud.cursor, spa_buf);
        // Publish the live overlay so pointer-only motion on a static desktop still
        // moves. Skip when `overlay()` is `None`: gamescope has no `SPA_META_Cursor`,
        // and writing `None` at frame rate would clobber the XFixes `Some` in this
        // same slot (pointer strobes). Hidden is still `Some(visible:false)`.
        if let Some(overlay) = ud.cursor.overlay() {
            if let Ok(mut slot) = ud.cursor_slot.live.lock() {
                *slot = Some(overlay);
            }
        }

        // SAFETY: `spa_buf` is the `spa_buffer` of `newest`, still held.
        let header = unsafe { read_buffer_header(spa_buf) };
        if header.stale() {
            ud.dbg_log_n += 1;
            if ud.dbg_log_n.is_power_of_two() {
                tracing::debug!(
                    skipped = ud.dbg_log_n,
                    drained,
                    "capture: skipped a stale CORRUPTED/cursor buffer (GNOME)"
                );
            }
            return;
        }

        // SAFETY: `spa_buf` is the `spa_buffer` of `newest`, still held.
        let damage = unsafe { damaged_area(spa_buf) };
        if !ud.damage.is_new_picture(damage, std::time::Instant::now()) {
            ud.undamaged += 1;
            return;
        }
        if let Some(p) = &ud.pacer {
            p.on_paint();
        }
        consume_frame(ud, spa_buf, newest, stream, header.pts);
    }));
    // A panic after publish still leaves the hold live, and the ledger leaves the requeue
    // to it.
    // SAFETY: all reads of `spa_buf`/`newest` (update_cursor_meta, consume_frame) completed
    // inside the closure above; `newest` was dequeued from this stream and not yet requeued.
    unsafe { ud.ledger.done(stream, newest) };
    if outcome.is_err() {
        // `.process` is per-frame; a deterministic panic would flood. Power-of-two throttle.
        static PANICS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = PANICS.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() {
            tracing::error!(
                count = n,
                "panic in pipewire process callback — frame dropped"
            );
        }
    }
}

/// Which arrivals are pictures, by the damage the producer reports.
///
/// KWin records on every repaint it schedules, and one comes ahead of each frame a browser
/// draws: that record repaints nothing and shows the picture before. Encoded, it is a repeat
/// in the stream and costs the new picture its slot.
pub(super) struct DamageGate {
    /// This producer has reported damage: only then does "none" mean none.
    reports: bool,
    last_picture: Option<std::time::Instant>,
    /// One and a half stream intervals. Past it the host repeats its picture anyway, so an
    /// arrival is taken whatever its damage says: damage reported wrongly costs a third of
    /// the rate, not the stream.
    within: std::time::Duration,
}

impl DamageGate {
    pub(super) fn new(interval: std::time::Duration) -> DamageGate {
        DamageGate {
            reports: false,
            last_picture: None,
            within: interval * 3 / 2,
        }
    }

    /// `damage` is [`damaged_area`]'s. `false`: the arrival repeats the picture before.
    fn is_new_picture(&mut self, damage: Option<u64>, now: std::time::Instant) -> bool {
        let recent = self
            .last_picture
            .is_some_and(|t| now.duration_since(t) < self.within);
        if damage == Some(0) && self.reports && recent {
            return false;
        }
        self.reports |= damage.is_some_and(|a| a > 0);
        self.last_picture = Some(now);
        true
    }
}

/// Pixels the producer repainted in this buffer, by its `SPA_META_VideoDamage` regions.
/// `None`: the buffer carries none, and the producer does not say.
///
/// # Safety
/// `spa_buf` is the `spa_buffer` of a buffer this `.process` still holds.
unsafe fn damaged_area(spa_buf: *mut spa::sys::spa_buffer) -> Option<u64> {
    // SAFETY: the caller holds the buffer. `find_meta` yields the region's real size, which
    // bounds every read below; the producer need not align its regions.
    unsafe {
        let meta = spa::sys::spa_buffer_find_meta(spa_buf, spa::sys::SPA_META_VideoDamage);
        if meta.is_null() || (*meta).data.is_null() {
            return None;
        }
        let regions = (*meta).data as *const spa::sys::spa_meta_region;
        let count = (*meta).size as usize / std::mem::size_of::<spa::sys::spa_meta_region>();
        let mut area = 0u64;
        for i in 0..count {
            let size = regions.add(i).read_unaligned().region.size;
            // The list ends at the first region of no size.
            if size.width == 0 || size.height == 0 {
                break;
            }
            area += u64::from(size.width) * u64::from(size.height);
        }
        Some(area)
    }
}

/// Dequeue to the newest buffer and how many arrived. Mutter bursts, and the older queued
/// buffers are stale: each is requeued after its cursor meta is read. `None` when nothing
/// was queued.
fn drain_to_newest(
    stream: &dyn StreamBuffers,
    ud: &mut UserData,
) -> Option<(*mut pw::sys::pw_buffer, u32)> {
    // SAFETY: `stream` is the live stream whose `.process` callback runs this on the loop
    // thread; `dequeue` returns a stream-owned `*mut pw_buffer` or null (null-checked),
    // single-threaded so no concurrent access.
    let mut newest = unsafe { stream.dequeue() };
    if newest.is_null() {
        return None;
    }
    let mut drained = 1u32;
    loop {
        // SAFETY: same stream/loop-thread contract; returns the next stream-owned buffer or null.
        let next = unsafe { stream.dequeue() };
        if next.is_null() {
            break;
        }
        // A new cursor bitmap rides only the buffer of the shape change; read it before
        // the stale pixels go back. Not while gated: that meta is in the doomed size.
        if ud.expect_dims.is_none() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: `newest` is dequeued and not yet requeued, as below.
                update_cursor_meta(&mut ud.cursor, unsafe { (*newest).buffer });
            }));
        }
        // SAFETY: `newest` was dequeued from this stream and not yet requeued; we immediately
        // overwrite it, so the requeued pointer is never touched again.
        unsafe { ud.requeue_unpublished(stream, newest) };
        newest = next;
        drained += 1;
    }
    Some((newest, drained))
}

/// Renegotiation normally lands within a frame or two; past this, stop starving the pipeline
/// (the real mode never applied).
const GATE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// Sacrificial birth mode (kwin.rs `create`): frame and cursor meta are in the doomed size
/// until renegotiation, so `false` holds this buffer back. Self-disarms on the expected size,
/// or after [`GATE_DEADLINE`]: degraded dims beat a first-frame-timeout retry loop if the
/// promised renegotiation never comes.
fn birth_gate(ud: &mut UserData) -> bool {
    let Some((ew, eh)) = ud.expect_dims else {
        return true;
    };
    let sz = ud.info.size();
    if sz.width == ew && sz.height == eh {
        tracing::info!(
            skipped = ud.gate_skips,
            width = ew,
            height = eh,
            "producer renegotiated to the expected mode — frames flow"
        );
        ud.expect_dims = None;
        return true;
    }
    if ud
        .gate_since
        .get_or_insert_with(std::time::Instant::now)
        .elapsed()
        > GATE_DEADLINE
    {
        tracing::warn!(
            negotiated_w = sz.width,
            negotiated_h = sz.height,
            expected_w = ew,
            expected_h = eh,
            skipped = ud.gate_skips,
            "producer never renegotiated to the expected mode — accepting its \
             dims (session runs degraded rather than wedged)"
        );
        ud.expect_dims = None;
        return true;
    }
    ud.gate_skips += 1;
    if ud.gate_skips == 1 || ud.gate_skips.is_power_of_two() {
        tracing::info!(
            negotiated_w = sz.width,
            negotiated_h = sz.height,
            expected_w = ew,
            expected_h = eh,
            n = ud.gate_skips,
            "holding frames until the producer renegotiates to the expected mode"
        );
    }
    false
}

/// The header meta and first chunk of a held buffer, for the stale-frame skip.
struct BufHeader {
    /// `SPA_META_Header` flags; 0 when the producer sends no header (it is optional).
    flags: u32,
    /// The compositor's stamp, upstream of the delivery jitter `SystemTime::now()` cannot
    /// see. Whether it is worth shipping is what the provenance line measures.
    pts: Option<i64>,
    chunk_size: u32,
    chunk_flags: i32,
    is_dmabuf: bool,
}

impl BufHeader {
    /// Mutter's pointer motion sends metadata-only CORRUPTED buffers (chunk size 0) that still
    /// reference a recycled old frame; encoding one is the flash. A dmabuf legitimately
    /// reports chunk size 0, so the size-0 skip is SHM-only.
    fn stale(&self) -> bool {
        let corrupted = (self.flags & spa::sys::SPA_META_HEADER_FLAG_CORRUPTED) != 0
            || (self.chunk_flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32) != 0;
        corrupted || (self.chunk_size == 0 && !self.is_dmabuf)
    }
}

/// # Safety
/// `spa_buf` is the `spa_buffer` of a buffer this `.process` still holds.
unsafe fn read_buffer_header(spa_buf: *mut spa::sys::spa_buffer) -> BufHeader {
    // SAFETY: `spa_buf` is held for this call (the caller's contract).
    // `spa_buffer_find_meta_data` scans its metadata for a `SPA_META_Header` of at least
    // `size_of::<spa_meta_header>()` bytes and returns a pointer into it or null, so `.flags`
    // and `.pts` are in bounds whenever it is non-null. The chunk reads are guarded in order —
    // `spa_buf`, `n_datas > 0`, the `datas` array, the first chunk — before any field load.
    // Single-threaded loop, nothing mutated.
    unsafe {
        let hdr = spa::sys::spa_buffer_find_meta_data(
            spa_buf,
            spa::sys::SPA_META_Header,
            std::mem::size_of::<spa::sys::spa_meta_header>(),
        ) as *const spa::sys::spa_meta_header;
        let (flags, pts) = if hdr.is_null() {
            (0u32, None)
        } else {
            ((*hdr).flags, Some((*hdr).pts))
        };
        let (chunk_size, chunk_flags, is_dmabuf) = if !spa_buf.is_null()
            && (*spa_buf).n_datas > 0
            && !(*spa_buf).datas.is_null()
            && !(*(*spa_buf).datas).chunk.is_null()
        {
            let d0 = (*spa_buf).datas;
            let c = (*d0).chunk;
            (
                (*c).size,
                (*c).flags,
                (*d0).type_ == spa::sys::SPA_DATA_DmaBuf,
            )
        } else {
            (0u32, 0i32, false)
        };
        BufHeader {
            flags,
            pts,
            chunk_size,
            chunk_flags,
            is_dmabuf,
        }
    }
}

/// The pacer's loop sources: the cap timer it re-arms, its RequestProcess hook, and the
/// heartbeat. Fields drop in order, and all three before the stream.
struct PacerSources<'l> {
    _heartbeat: pw::loop_::TimerSource<'l>,
    _requests: RequestListener,
    _cap: pw::loop_::TimerSource<'l>,
}

/// Wire the pacer to the loop: the cap timer its `schedule` re-arms, the RequestProcess and
/// `trigger_done` hook, and the heartbeat that re-reads the driver role.
fn install_pacer<'l>(
    loop_: &'l pw::loop_::Loop,
    stream: &pw::stream::Stream,
    pacer: &Rc<Pacer>,
    shared: StreamShared,
) -> PacerSources<'l> {
    let cap = {
        let p = pacer.clone();
        loop_.add_timer(move |_| p.schedule())
    };
    {
        use pw::loop_::IsSource;
        pacer.timer.set(Some(RawTimer {
            utils: loop_.as_raw().utils,
            source: cap.as_ptr(),
        }));
    }
    let requests = RequestListener::attach(stream, pacer.clone());
    let heartbeat = {
        let p = pacer.clone();
        loop_.add_timer(move |_| {
            // Re-read the role: PipeWire may assign the driver after the Streaming edge. The
            // producer's report, not the broken latch: the pacer follows the stream.
            let StreamStatus::Streaming { driving: was } = shared.reported() else {
                return;
            };
            // SAFETY: the stream outlives this timer source (dropped before it).
            let driving = unsafe { pw::sys::pw_stream_is_driving(p.stream.get()) };
            if driving != was {
                shared.report(StreamStatus::Streaming { driving });
                p.on_streaming(driving);
            }
            if driving {
                p.heartbeat();
            }
        })
    };
    let _ = heartbeat.update_timer(Some(HEARTBEAT), Some(HEARTBEAT));
    PacerSources {
        _heartbeat: heartbeat,
        _requests: requests,
        _cap: cap,
    }
}

/// The ledger's loop sources: the undelivered sweep, then the requeue receiver. Fields drop in
/// order, and both before the stream.
struct LedgerSources<'l> {
    _undelivered: Option<pw::loop_::TimerSource<'l>>,
    _requeue: pw::channel::AttachedReceiver<'l, ()>,
}

/// Wire the ledger to the loop: the receiver where a withheld buffer rejoins, and, with
/// explicit sync (`sweep`), the timer that recovers buffers that never arrived. Holds on
/// published frames park their release from whichever thread drops last and wake `requeue_rx`;
/// a withheld buffer rejoins only on the loop thread — here or in `try_defer`.
fn install_ledger<'l>(
    loop_: &'l pw::loop_::Loop,
    stream: &pw::stream::Stream,
    ledger: &Arc<PoolLedger>,
    requeue_rx: pw::channel::Receiver<()>,
    sweep: bool,
) -> LedgerSources<'l> {
    // A `BufferHold` dropping on any thread only parks and wakes; this loop-thread callback
    // (or `try_defer`, whichever runs first) is where a withheld buffer rejoins.
    let ledger_cb = ledger.clone();
    let stream_ptr = stream.as_raw_ptr() as usize;
    let requeue = requeue_rx.attach(loop_, move |()| {
        // SAFETY: the loop thread dispatches this. The stream outlives these sources (bound
        // after it, dropped before it), and the loop stops dispatching once `run()` returns.
        unsafe { ledger_cb.drain(&(stream_ptr as *mut pw::sys::pw_stream)) };
    });
    // On a timer, not from `.process`: a producer whose whole pool went undelivered sends
    // nothing more, and `.process` never runs again.
    let undelivered = sweep.then(|| {
        let ledger = ledger.clone();
        // SAFETY: the loop thread dispatches this between callbacks, and the ledger lists only
        // buffers the stream still has.
        let timer = loop_.add_timer(move |_| unsafe { ledger.recover_undelivered() });
        let _ = timer.update_timer(Some(UNDELIVERED_SWEEP), Some(UNDELIVERED_SWEEP));
        timer
    });
    LedgerSources {
        _undelivered: undelivered,
        _requeue: requeue,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_params, resolve_offer, resolved_capture_arm, DeviceModifiers, NegotiationPlan, Offer,
        ProducerProbe, StreamConfig, SPA_VIDEO_TRANSFER_SMPTE2084,
    };
    use crate::knobs::Knobs;
    use crate::linux::pipewire::plan::{negotiation_plan, ImportPolicy, NegotiationInputs};
    use crate::linux::pipewire::CaptureOpts;
    use crate::linux::sync_timeline::PARAM_BUFFERS_META_TYPE;
    use crate::{PixelFormat, ZeroCopyPolicy};
    use pf_zerocopy::Ladder;
    use pipewire::spa::param::video::VideoFormat;
    use pipewire::spa::param::ParamType;
    use pipewire::spa::pod::{deserialize::PodDeserializer, ChoiceValue, Value};
    use pipewire::spa::sys as spa_sys;
    use pipewire::spa::utils::{Choice, ChoiceEnum, Id};

    /// A producer that fills in no damage loses no frame, and one that reports none for a
    /// scene that changed still streams, at two pictures in three.
    #[test]
    fn only_reported_damage_gates_an_arrival() {
        use super::DamageGate;
        let t = std::time::Instant::now();
        let ms = std::time::Duration::from_millis;
        let mut gate = DamageGate::new(ms(16));
        assert!(gate.is_new_picture(Some(0), t), "the first picture");
        assert!(
            gate.is_new_picture(Some(0), t + ms(8)),
            "it may never report any"
        );
        assert!(gate.is_new_picture(None, t + ms(16)));
        assert!(gate.is_new_picture(Some(64), t + ms(24)));
        assert!(
            !gate.is_new_picture(Some(0), t + ms(38)),
            "a record of the unchanged scene, ahead of the next frame"
        );
        assert!(gate.is_new_picture(Some(64), t + ms(40)));
        assert!(!gate.is_new_picture(Some(0), t + ms(56)));
        assert!(
            gate.is_new_picture(Some(0), t + ms(64)),
            "the host would repeat its picture by now"
        );
    }

    /// Each param's id (EnumFormat, Buffers, Meta), in connect order.
    fn param_ids(params: &[Vec<u8>]) -> Vec<u32> {
        params
            .iter()
            .map(|p| match PodDeserializer::deserialize_any_from(p) {
                Ok((_, Value::Object(o))) => o.id,
                _ => panic!("a param that is not an object pod"),
            })
            .collect()
    }

    fn opts() -> CaptureOpts {
        CaptureOpts {
            allow_zerocopy: true,
            want_444: false,
            want_hdr: false,
            ten_bit_sdr: false,
            sdr10_native: false,
            full_range: true,
            expect_exact_dims: false,
            cursor_id0_hides: false,
            producer_is_gamescope: false,
            pool_min: crate::POOL_MIN,
            pool_max: None,
            unpaced: false,
            lazy: false,
            stream_hz: 0,
            knobs: Default::default(),
        }
    }

    fn plan() -> NegotiationPlan {
        NegotiationPlan {
            build_importer: true,
            import_policy: ImportPolicy::default(),
            nvenc_raw: false,
            vaapi_passthrough: false,
            prefer_native_nv12: false,
            prefer_native_p010: false,
            force_shm: false,
            raw_dmabuf_latched: false,
            gpu_import_latched: false,
        }
    }

    fn cfg(opts: CaptureOpts) -> StreamConfig {
        StreamConfig {
            node_id: 0,
            opts,
            preferred: None,
            policy: ZeroCopyPolicy::default(),
            plan: plan(),
        }
    }

    fn dmabuf_offer() -> Offer {
        Offer {
            importer: false,
            modifiers: vec![0],
            modifiers_bgra: vec![0],
            hdr_modifiers: Vec::new(),
            extend_pyrowave: false,
            want_dmabuf: true,
        }
    }

    /// The format pods lead, the explicit-sync Buffers twin precedes the plain one, and the
    /// header and damage metas ride behind the metas they join.
    #[test]
    fn params_go_formats_then_buffers_then_metas() {
        let (format, buffers, meta) = (
            ParamType::EnumFormat.as_raw(),
            ParamType::Buffers.as_raw(),
            ParamType::Meta.as_raw(),
        );
        let probe = ProducerProbe::default();
        let p = build_params(&cfg(opts()), &probe, &dmabuf_offer(), true).unwrap();
        assert_eq!(
            param_ids(&p),
            [format, format, buffers, buffers, meta, meta, meta, meta],
            "BGRx + BGRA, sync twin + plain Buffers, cursor + sync + header + damage metas"
        );
        let gamescope = CaptureOpts {
            producer_is_gamescope: true,
            ..opts()
        };
        let p = build_params(&cfg(gamescope), &probe, &dmabuf_offer(), false);
        assert_eq!(
            param_ids(&p.unwrap()),
            [format, format, buffers],
            "gamescope sends no cursor meta, so no meta at all"
        );
    }

    /// An unpaced offer repeats the format set behind itself; the CPU path offers one pod.
    #[test]
    fn unpaced_offers_both_format_sets() {
        let unpaced = CaptureOpts {
            unpaced: true,
            ..opts()
        };
        let probe = ProducerProbe::default();
        let p = build_params(&cfg(unpaced), &probe, &dmabuf_offer(), false);
        assert_eq!(param_ids(&p.unwrap()).len(), 4 + 4);
        let cpu = Offer {
            want_dmabuf: false,
            ..dmabuf_offer()
        };
        let p = build_params(&cfg(opts()), &probe, &cpu, false).unwrap();
        assert_eq!(param_ids(&p).len(), 1 + 4);
    }

    /// The producer's NV12 pod, which goes first, pins the session's range.
    #[test]
    fn the_nv12_pod_pins_the_session_range() {
        let key = spa_sys::SPA_FORMAT_VIDEO_colorRange.to_ne_bytes();
        for (full_range, want) in [
            (true, spa_sys::SPA_VIDEO_COLOR_RANGE_0_255),
            (false, spa_sys::SPA_VIDEO_COLOR_RANGE_16_235),
        ] {
            let stream = StreamConfig {
                plan: NegotiationPlan {
                    prefer_native_nv12: true,
                    ..plan()
                },
                ..cfg(CaptureOpts {
                    full_range,
                    ..opts()
                })
            };
            let probe = ProducerProbe::default();
            let p = build_params(&stream, &probe, &dmabuf_offer(), false).unwrap();
            let nv12 = &p[0];
            // Property = { key, flags, value_pod }; value_pod = { size, type, Id body }.
            let at = nv12.windows(4).position(|w| w == key).expect("colorRange");
            let id = u32::from_ne_bytes(nv12[at + 16..at + 20].try_into().unwrap());
            assert_eq!(id, want, "full_range={full_range}");
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Producer {
        Kwin,
        Gamescope,
        Mutter,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Lane {
        Vaapi,
        NvencRaw,
        Cuda,
        Cpu,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Depth {
        Sdr,
        Hdr,
        Sdr10,
    }

    use Depth::{Hdr, Sdr, Sdr10};
    use Lane::{Cpu, Cuda, NvencRaw, Vaapi};
    use Producer::{Gamescope, Kwin, Mutter};

    /// NVIDIA block-linear the fused convert samples, a compressed layout it does not, and the
    /// AMD tiling a VA encoder proved. LINEAR reads `L`.
    const A: u64 = 0x0300_0000_0060_6010;
    const B: u64 = 0x0300_0000_00e0_8014;
    const T: u64 = 0x0200_0000_1974_0b04;

    /// One cell as `from_virtual_output` and `spawn_pipewire` resolve it on a healthy box:
    /// shipped knobs, no latch, 1080p60, KWin 6.8, a Mutter that requests, an importer that
    /// lists `A` and `B`, a fused convert that samples `A`, a syncobj device.
    fn cell(
        producer: Producer,
        lane: Lane,
        depth: Depth,
    ) -> (StreamConfig, ProducerProbe, DeviceModifiers) {
        let (kwin, gamescope, gpu) = (producer == Kwin, producer == Gamescope, lane != Cpu);
        let knobs = Knobs {
            explicit_sync: true,
            lazy_capture: true,
            pipewire_nv12: true,
            header_pts: true,
            zerocopy_hold: true,
            ..Knobs::default()
        };
        let opts = CaptureOpts {
            allow_zerocopy: gpu,
            want_444: false,
            want_hdr: depth == Hdr,
            ten_bit_sdr: depth == Sdr10,
            sdr10_native: depth == Sdr10 && gamescope,
            full_range: true,
            expect_exact_dims: kwin,
            cursor_id0_hides: kwin,
            producer_is_gamescope: gamescope,
            pool_min: if kwin {
                crate::KWIN_POOL_MIN
            } else {
                crate::POOL_MIN
            },
            pool_max: kwin.then_some(crate::KWIN_POOL_MAX),
            unpaced: kwin && !knobs.kwin_paced,
            lazy: knobs.lazy_capture,
            stream_hz: 60,
            knobs,
        };
        let formats = [
            PixelFormat::Bgrx,
            PixelFormat::Bgra,
            PixelFormat::X2Bgr10,
            PixelFormat::X2Rgb10,
        ];
        let policy = ZeroCopyPolicy {
            backend_is_vaapi: lane == Vaapi,
            backend_is_gpu: gpu,
            native_nv12_session: matches!(lane, Vaapi | NvencRaw),
            hdr_cuda_ok: matches!(lane, NvencRaw | Cuda),
            nvenc_raw_dmabuf: lane == NvencRaw,
            gamescope_tiled: gamescope,
            // The host proves encoder modifiers only for a tiled gamescope.
            encoder_modifiers: if gamescope && lane == Vaapi {
                let proved = |f| (pf_frame::drm_fourcc(f).unwrap(), vec![T]);
                formats.map(proved).to_vec()
            } else {
                Vec::new()
            },
            ..ZeroCopyPolicy::default()
        };
        let plan = negotiation_plan(NegotiationInputs {
            zerocopy: opts.allow_zerocopy,
            force_shm: false,
            want_hdr: opts.want_hdr,
            sdr10_native: opts.sdr10_native,
            want_444: false,
            backend_is_vaapi: policy.backend_is_vaapi,
            pyrowave_session: false,
            native_nv12_session: policy.native_nv12_session,
            ladder: Ladder::default(),
            native_nv12_knob: opts.knobs.pipewire_nv12,
            hdr_cuda_ok: policy.hdr_cuda_ok,
            nv12_knob: true,
            nvenc_raw: policy.nvenc_raw_dmabuf,
        });
        let probe = ProducerProbe {
            supports_request: producer == Mutter,
            framerate_mhz: kwin,
        };
        let importer = plan.build_importer;
        let lists = DeviceModifiers {
            importer,
            packed: [vec![A, B], vec![A, B]].map(|l| if importer { l } else { Vec::new() }),
            packed_fused: plan.nvenc_raw.then(|| [vec![A], vec![A]]),
            ten_bit: importer.then(|| [(vec![A, B], vec![A]), (vec![A, B], vec![A])]),
        };
        let cfg = StreamConfig {
            node_id: 0,
            opts,
            preferred: Some((1920, 1080, 60)),
            policy,
            plan,
        };
        (cfg, probe, lists)
    }

    fn format_name(format: VideoFormat) -> &'static str {
        match format {
            VideoFormat::BGRx => "BGRx",
            VideoFormat::BGRA => "BGRA",
            VideoFormat::NV12 => "NV12",
            VideoFormat::P010_10LE => "P010",
            VideoFormat::xBGR_210LE => "xBGR10",
            VideoFormat::xRGB_210LE => "xRGB10",
            _ => "?",
        }
    }

    fn modifier_name(modifier: &i64) -> &'static str {
        match *modifier as u64 {
            0 => "L",
            A => "A",
            B => "B",
            T => "T",
            _ => "?",
        }
    }

    /// `formats | buffers | metas`, in connect order. A format reads layout, transfer, its
    /// modifiers and its `maxFramerate`; `RGB*` is the CPU path's choice of packed layouts.
    fn describe(params: &[Vec<u8>]) -> String {
        let mut groups: [Vec<String>; 3] = Default::default();
        for p in params {
            let Ok((_, Value::Object(o))) = PodDeserializer::deserialize_any_from(p) else {
                panic!("a param that is not an object pod");
            };
            let prop = |key: u32| o.properties.iter().find(|p| p.key == key).map(|p| &p.value);
            if o.id == ParamType::EnumFormat.as_raw() {
                let mut pod = match prop(spa_sys::SPA_FORMAT_VIDEO_format) {
                    Some(Value::Id(Id(f))) => format_name(VideoFormat::from_raw(*f)).to_string(),
                    _ => "RGB*".to_string(),
                };
                match prop(spa_sys::SPA_FORMAT_VIDEO_transferFunction) {
                    Some(Value::Id(Id(SPA_VIDEO_TRANSFER_SMPTE2084))) => pod += "/pq",
                    Some(_) => pod += "/srgb",
                    None => {}
                }
                if let Some(Value::Choice(ChoiceValue::Long(Choice(
                    _,
                    ChoiceEnum::Enum { alternatives, .. },
                )))) = prop(spa_sys::SPA_FORMAT_VIDEO_modifier)
                {
                    let mods: Vec<_> = alternatives.iter().map(modifier_name).collect();
                    pod += &format!("({})", mods.join(","));
                }
                if let Some(Value::Fraction(f)) = prop(spa_sys::SPA_FORMAT_VIDEO_maxFramerate) {
                    pod += &format!("@{}", f.num);
                }
                groups[0].push(pod);
            } else if o.id == ParamType::Buffers.as_raw() {
                let Some(Value::Int(types)) = prop(spa_sys::SPA_PARAM_BUFFERS_dataType) else {
                    panic!("a Buffers pod without data types");
                };
                let dmabuf = 1i32 << spa_sys::SPA_DATA_DmaBuf;
                let kind = match *types {
                    t if t == dmabuf => "dmabuf",
                    t if t & dmabuf == 0 => "shm",
                    _ => "mappable",
                };
                let sync = prop(PARAM_BUFFERS_META_TYPE).map_or("", |_| "+sync");
                groups[1].push(format!("{kind}{sync}"));
            } else {
                let Some(Value::Id(Id(kind))) = prop(spa_sys::SPA_PARAM_META_type) else {
                    panic!("a Meta pod without a type");
                };
                let meta = match *kind {
                    spa_sys::SPA_META_Cursor => "cursor",
                    spa_sys::SPA_META_SyncTimeline => "sync",
                    spa_sys::SPA_META_Header => "header",
                    spa_sys::SPA_META_VideoDamage => "damage",
                    _ => "?",
                };
                groups[2].push(meta.to_string());
            }
        }
        let groups = groups.map(|g| {
            if g.is_empty() {
                "-".into()
            } else {
                g.join(" ")
            }
        });
        groups.join(" | ")
    }

    /// Every producer × lane × depth: the arm the plan resolves, then the pods in connect
    /// order. KWin offers its unpaced set ahead of the plain one; gamescope takes no cursor
    /// meta. On the CPU lane the plan gives HDR no GPU arm (`cpu`), and its offer stays LINEAR
    /// 10-bit dmabufs: SHM carries 8 bits.
    #[rustfmt::skip]
    const MATRIX: &[(Producer, Lane, Depth, &str, &str)] = &[
        (Kwin, Vaapi, Sdr, "dmabuf-passthrough", "NV12(L)@60 BGRx(L)@60 BGRA(L)@60 NV12(L) BGRx(L) BGRA(L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Kwin, Vaapi, Hdr, "dmabuf-passthrough", "P010/pq(L)@60 xBGR10/pq(L)@60 xRGB10/pq(L)@60 P010/pq(L) xBGR10/pq(L) xRGB10/pq(L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Kwin, Vaapi, Sdr10, "dmabuf-passthrough", "NV12(L)@60 BGRx(L)@60 BGRA(L)@60 NV12(L) BGRx(L) BGRA(L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Kwin, NvencRaw, Sdr, "dmabuf-to-encoder", "NV12(L)@60 BGRx(A,L)@60 BGRA(A,L)@60 NV12(L) BGRx(A,L) BGRA(A,L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Kwin, NvencRaw, Hdr, "dmabuf-to-encoder", "P010/pq(L)@60 xBGR10/pq(L)@60 xRGB10/pq(L)@60 P010/pq(L) xBGR10/pq(L) xRGB10/pq(L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Kwin, NvencRaw, Sdr10, "dmabuf-to-encoder", "NV12(L)@60 BGRx(A,L)@60 BGRA(A,L)@60 NV12(L) BGRx(A,L) BGRA(A,L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Kwin, Cuda, Sdr, "cuda-import", "BGRx(A,B,L)@60 BGRA(A,B,L)@60 BGRx(A,B,L) BGRA(A,B,L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Kwin, Cuda, Hdr, "cuda-import", "xBGR10/pq(L)@60 xRGB10/pq(L)@60 xBGR10/pq(L) xRGB10/pq(L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Kwin, Cuda, Sdr10, "cuda-import", "BGRx(A,B,L)@60 BGRA(A,B,L)@60 BGRx(A,B,L) BGRA(A,B,L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Kwin, Cpu, Sdr, "cpu", "RGB*@0 RGB* | mappable | cursor header damage"),
        (Kwin, Cpu, Hdr, "cpu", "xBGR10/pq(L)@60 xRGB10/pq(L)@60 xBGR10/pq(L) xRGB10/pq(L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Kwin, Cpu, Sdr10, "cpu", "RGB*@0 RGB* | mappable | cursor header damage"),
        (Gamescope, Vaapi, Sdr, "dmabuf-passthrough", "NV12(L)@60 BGRx(T,L)@60 BGRA(T,L)@60 | dmabuf+sync dmabuf | sync header damage"),
        (Gamescope, Vaapi, Hdr, "dmabuf-passthrough", "P010/pq(L)@60 xBGR10/pq(T,L)@60 xRGB10/pq(T,L)@60 | dmabuf+sync dmabuf | sync header damage"),
        (Gamescope, Vaapi, Sdr10, "dmabuf-passthrough", "P010/srgb(L)@60 xBGR10/srgb(T,L)@60 xRGB10/srgb(T,L)@60 BGRx(T,L)@60 BGRA(T,L)@60 | dmabuf+sync dmabuf | sync header damage"),
        (Gamescope, NvencRaw, Sdr, "dmabuf-to-encoder", "NV12(L)@60 BGRx(A,L)@60 BGRA(A,L)@60 | dmabuf+sync dmabuf | sync header damage"),
        (Gamescope, NvencRaw, Hdr, "dmabuf-to-encoder", "P010/pq(L)@60 xBGR10/pq(A,L)@60 xRGB10/pq(A,L)@60 | dmabuf+sync dmabuf | sync header damage"),
        (Gamescope, NvencRaw, Sdr10, "dmabuf-to-encoder", "P010/srgb(L)@60 xBGR10/srgb(A,L)@60 xRGB10/srgb(A,L)@60 BGRx(A,L)@60 BGRA(A,L)@60 | dmabuf+sync dmabuf | sync header damage"),
        (Gamescope, Cuda, Sdr, "cuda-import", "BGRx(A,B,L)@60 BGRA(A,B,L)@60 | dmabuf+sync dmabuf | sync header damage"),
        (Gamescope, Cuda, Hdr, "cuda-import", "xBGR10/pq(L)@60 xRGB10/pq(L)@60 | dmabuf+sync dmabuf | sync header damage"),
        (Gamescope, Cuda, Sdr10, "cuda-import", "xBGR10/srgb(L)@60 xRGB10/srgb(L)@60 BGRx(A,B,L)@60 BGRA(A,B,L)@60 | dmabuf+sync dmabuf | sync header damage"),
        (Gamescope, Cpu, Sdr, "cpu", "RGB* | mappable | -"),
        (Gamescope, Cpu, Hdr, "cpu", "xBGR10/pq(L)@60 xRGB10/pq(L)@60 | dmabuf+sync dmabuf | sync header damage"),
        (Gamescope, Cpu, Sdr10, "cpu", "RGB* | mappable | -"),
        (Mutter, Vaapi, Sdr, "dmabuf-passthrough", "NV12(L) BGRx(L) BGRA(L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Mutter, Vaapi, Hdr, "dmabuf-passthrough", "P010/pq(L) xBGR10/pq(L) xRGB10/pq(L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Mutter, Vaapi, Sdr10, "dmabuf-passthrough", "NV12(L) BGRx(L) BGRA(L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Mutter, NvencRaw, Sdr, "dmabuf-to-encoder", "NV12(L) BGRx(A,L) BGRA(A,L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Mutter, NvencRaw, Hdr, "dmabuf-to-encoder", "P010/pq(L) xBGR10/pq(L) xRGB10/pq(L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Mutter, NvencRaw, Sdr10, "dmabuf-to-encoder", "NV12(L) BGRx(A,L) BGRA(A,L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Mutter, Cuda, Sdr, "cuda-import", "BGRx(A,B,L) BGRA(A,B,L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Mutter, Cuda, Hdr, "cuda-import", "xBGR10/pq(L) xRGB10/pq(L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Mutter, Cuda, Sdr10, "cuda-import", "BGRx(A,B,L) BGRA(A,B,L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Mutter, Cpu, Sdr, "cpu", "RGB* | mappable | cursor header damage"),
        (Mutter, Cpu, Hdr, "cpu", "xBGR10/pq(L) xRGB10/pq(L) | dmabuf+sync dmabuf | cursor sync header damage"),
        (Mutter, Cpu, Sdr10, "cpu", "RGB* | mappable | cursor header damage"),
    ];

    #[test]
    fn the_offer_follows_the_producer_the_lane_and_the_depth() {
        let mut moved = Vec::new();
        for producer in [Kwin, Gamescope, Mutter] {
            for lane in [Vaapi, NvencRaw, Cuda, Cpu] {
                for depth in [Sdr, Hdr, Sdr10] {
                    let &(.., arm, pods) = MATRIX
                        .iter()
                        .find(|r| (r.0, r.1, r.2) == (producer, lane, depth))
                        .expect("every cell has a row");
                    let (cfg, probe, lists) = cell(producer, lane, depth);
                    let offer = resolve_offer(&cfg, &Ladder::default(), &lists);
                    let sync = cfg.wants_sync(offer.want_dmabuf);
                    let params = build_params(&cfg, &probe, &offer, sync).unwrap();
                    let got_arm =
                        resolved_capture_arm(&cfg.plan, offer.importer, offer.want_dmabuf);
                    let got = describe(&params);
                    if (got_arm.as_str(), got.as_str()) != (arm, pods) {
                        moved.push(format!(
                            "({producer:?}, {lane:?}, {depth:?}, {:?}, {got:?}),",
                            got_arm.as_str()
                        ));
                    }
                }
            }
        }
        assert_eq!(MATRIX.len(), 3 * 4 * 3, "one row per cell");
        assert!(moved.is_empty(), "cells that moved:\n{}", moved.join("\n"));
    }
}

/// The stream callbacks with no daemon behind them: the stream is two lists, and the
/// buffers are laid out by hand the way libpipewire hands them over.
#[cfg(test)]
mod callbacks {
    use super::{
        on_add_buffer, on_param_changed, on_process, on_remove_buffer, realtime_minus_monotonic_ns,
        serialize_pod, video_raw, CursorState, DamageGate, Extent, ImportState, Pacing,
        PassthroughFallbacks, Pod, PoolCensus, StreamBuffers, StreamShared, StreamStatus, UserData,
        SPA_VIDEO_TRANSFER_SMPTE2084,
    };
    use crate::linux::pipewire::ledger::PoolLedger;
    use crate::linux::pipewire::plan::ImportPolicy;
    use crate::{CapturedFrame, FramePayload, PixelFormat};
    use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
    use pipewire::spa::param::ParamType;
    use pipewire::spa::pod::Value;
    use pipewire::spa::sys as spa_sys;
    use pipewire::sys::pw_buffer;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::os::fd::IntoRawFd as _;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A stream as two lists: what the producer sent, and what it got back.
    #[derive(Default)]
    struct Lists {
        sent: RefCell<VecDeque<*mut pw_buffer>>,
        back: RefCell<Vec<*mut pw_buffer>>,
    }

    impl StreamBuffers for Lists {
        // unsafe-fn-no-op-ok: the trait's signature; a list keeps no contract.
        unsafe fn dequeue(&self) -> *mut pw_buffer {
            let next = self.sent.borrow_mut().pop_front();
            next.unwrap_or(std::ptr::null_mut())
        }

        // unsafe-fn-no-op-ok: the trait's signature; a list keeps no contract.
        unsafe fn queue(&self, buf: *mut pw_buffer) {
            self.back.borrow_mut().push(buf);
        }
    }

    fn leak<T>(v: T) -> *mut T {
        Box::leak(Box::new(v))
    }

    fn pixels(len: usize) -> Vec<u8> {
        (0..len).map(|i| i as u8).collect()
    }

    fn chunk(size: u32, stride: i32) -> spa_sys::spa_chunk {
        spa_sys::spa_chunk {
            offset: 0,
            size,
            stride,
            flags: 0,
        }
    }

    /// One plane of mapped memory holding `pixels`.
    fn memory(pixels: &[u8]) -> spa_sys::spa_data {
        spa_sys::spa_data {
            type_: spa_sys::SPA_DATA_MemPtr,
            flags: 0,
            fd: -1,
            mapoffset: 0,
            maxsize: pixels.len() as u32,
            data: pixels.to_vec().leak().as_mut_ptr().cast(),
            chunk: std::ptr::null_mut(),
        }
    }

    /// One pool buffer as libpipewire lays it out, leaked so every address in it stays
    /// put: a `pw_buffer` over a `spa_buffer` of one plane and a header meta.
    fn buffer(
        data: spa_sys::spa_data,
        chunk: spa_sys::spa_chunk,
        header_flags: u32,
    ) -> *mut pw_buffer {
        let header = spa_sys::spa_meta_header {
            flags: header_flags,
            offset: 0,
            pts: 0,
            dts_offset: 0,
            seq: 0,
        };
        let meta = spa_sys::spa_meta {
            type_: spa_sys::SPA_META_Header,
            size: std::mem::size_of::<spa_sys::spa_meta_header>() as u32,
            data: leak(header).cast(),
        };
        let data = spa_sys::spa_data {
            chunk: leak(chunk),
            ..data
        };
        let spa_buf = spa_sys::spa_buffer {
            n_metas: 1,
            n_datas: 1,
            metas: leak(meta),
            datas: leak(data),
        };
        // SAFETY: a `pw_buffer` is pointers and integers, so all-zero is one.
        let mut buf: pw_buffer = unsafe { std::mem::zeroed() };
        buf.buffer = leak(spa_buf);
        leak(buf)
    }

    /// A memory buffer of `pixels` at `stride`.
    fn shm(pixels: &[u8], stride: i32) -> *mut pw_buffer {
        buffer(memory(pixels), chunk(pixels.len() as u32, stride), 0)
    }

    /// A buffer that names an fd as its dmabuf, with the empty chunk a dmabuf reports.
    /// The fd is a file of `pixels`: short of the fence ioctl, which it refuses, the
    /// capture cannot tell.
    fn dmabuf(pixels: &[u8], stride: i32) -> *mut pw_buffer {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "pf-capture-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, pixels).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let data = spa_sys::spa_data {
            type_: spa_sys::SPA_DATA_DmaBuf,
            fd: i64::from(file.into_raw_fd()),
            maxsize: 0,
            data: std::ptr::null_mut(),
            ..memory(&[])
        };
        buffer(data, chunk(0, stride), 0)
    }

    /// 16x4 BGRx dmabufs at a 64-byte pitch, which every driver imports unrounded.
    fn dmabufs<const N: usize>() -> [*mut pw_buffer; N] {
        std::array::from_fn(|_| dmabuf(&pixels(256), 64))
    }

    /// The Format pod a producer fixates: one layout and one size, with the modifier
    /// and transfer function it names.
    fn format_pod(
        format: VideoFormat,
        (w, h): (u32, u32),
        modifier: Option<u64>,
        transfer: Option<u32>,
    ) -> Vec<u8> {
        use pipewire::spa::pod::{property, Property, PropertyFlags};
        let layout = property!(
            pipewire::spa::param::format::FormatProperties::VideoFormat,
            Id,
            format
        );
        let mut obj = video_raw(layout, Extent::Fixed(w, h), Pacing::Producer);
        let named = |key, value| Property {
            key,
            flags: PropertyFlags::empty(),
            value,
        };
        obj.properties.extend(
            modifier.map(|m| named(spa_sys::SPA_FORMAT_VIDEO_modifier, Value::Long(m as i64))),
        );
        obj.properties.extend(transfer.map(|t| {
            let id = Value::Id(pipewire::spa::utils::Id(t));
            named(spa_sys::SPA_FORMAT_VIDEO_transferFunction, id)
        }));
        serialize_pod(obj).unwrap()
    }

    /// One capture stream without its daemon: the listener's state, and the stream as
    /// lists.
    struct Rig {
        ud: UserData,
        stream: Lists,
        _wake: std::sync::mpsc::Receiver<()>,
        _requeue: pipewire::channel::Receiver<()>,
    }

    impl Rig {
        /// An active capture on the CPU lane that has negotiated nothing yet.
        fn new() -> Rig {
            static IDENTITY: AtomicU64 = AtomicU64::new(0x5e55_1200_0000);
            let (wake, _wake) = std::sync::mpsc::sync_channel(1);
            let (requeue, _requeue) = pipewire::channel::channel::<()>();
            let health = pf_zerocopy::zero_copy_health(IDENTITY.fetch_add(1, Relaxed));
            let shared = StreamShared::new(health);
            shared.active.store(true, Relaxed);
            let ledger = Arc::new(PoolLedger::new(requeue, None));
            let ud = UserData {
                info: VideoInfoRaw::default(),
                format: None,
                modifier: 0,
                queue: Default::default(),
                wake,
                shared,
                cursor_slot: Default::default(),
                importer: Default::default(),
                vaapi_passthrough: false,
                hdr_tiled_raw: false,
                import_policy: ImportPolicy::default(),
                import_state: ImportState::default(),
                dbg_log_n: 0,
                pts: crate::pts_provenance::PtsProvenance::new(),
                pts_reported: Instant::now(),
                rt_minus_mono_ns: realtime_minus_monotonic_ns(),
                hdr_pts_enabled: true,
                pool: PoolCensus::default(),
                passthrough_fallbacks: PassthroughFallbacks::default(),
                cursor: CursorState::new(false),
                expect_dims: None,
                gate_skips: 0,
                gate_since: None,
                ledger,
                pacer: None,
                held_drops: 0,
                damage: DamageGate::new(Duration::from_millis(16)),
                undamaged: 0,
                sync: None,
            };
            Rig {
                ud,
                stream: Lists::default(),
                _wake,
                _requeue,
            }
        }

        /// A raw-passthrough capture of 16x4 LINEAR BGRx whose producer allocated `pool`.
        fn raw_lane(pool: &[*mut pw_buffer]) -> Rig {
            let mut rig = Rig::new();
            rig.ud.vaapi_passthrough = true;
            rig.negotiate(VideoFormat::BGRx, (16, 4), 0);
            rig.allocate(pool);
            rig
        }

        /// The producer fixates `format` at `size`.
        fn negotiate(&mut self, format: VideoFormat, size: (u32, u32), modifier: u64) {
            let pod = format_pod(format, size, Some(modifier), None);
            let id = ParamType::Format.as_raw();
            on_param_changed(&mut self.ud, id, Pod::from_bytes(&pod));
        }

        /// The producer allocates `pool`.
        fn allocate(&mut self, pool: &[*mut pw_buffer]) {
            for &buf in pool {
                on_add_buffer(&mut self.ud, buf as usize);
            }
        }

        /// One `.process` with `arrivals` queued, oldest first.
        fn process(&mut self, arrivals: &[*mut pw_buffer]) {
            self.stream.sent.borrow_mut().extend(arrivals);
            on_process(&self.stream, &mut self.ud);
        }

        /// The frame the consumer would take now.
        fn take(&self) -> Option<CapturedFrame> {
            let mut queue = self.ud.queue.lock().unwrap();
            queue.take_ready(Instant::now()).map(|t| t.frame)
        }

        /// The loop servicing the wake of a dropped hold: how many buffers rejoined.
        fn service(&self) -> usize {
            // SAFETY: the lists are the stream, and every buffer on them is leaked.
            unsafe { self.ud.ledger.drain(&self.stream) }
        }

        /// The buffers handed back since the last look, in order.
        fn back(&self) -> Vec<*mut pw_buffer> {
            std::mem::take(&mut *self.stream.back.borrow_mut())
        }
    }

    /// A published buffer stays out of the pool until the encoder lets go of its frame,
    /// then rejoins once.
    #[test]
    fn a_held_buffer_rejoins_once_after_its_hold_drops() {
        let pool = dmabufs::<4>();
        let mut rig = Rig::raw_lane(&pool);
        rig.process(&pool[..1]);
        assert!(rig.back().is_empty(), "the hold owns the requeue");
        let frame = rig.take().expect("a frame");
        assert!(matches!(frame.payload, FramePayload::Dmabuf(_)));
        assert_eq!(rig.service(), 0, "the encoder still reads it");
        drop(frame);
        assert_eq!(rig.service(), 1);
        assert_eq!(rig.back(), [pool[0]]);
        assert_eq!(rig.service(), 0, "one hold, one requeue");
    }

    /// With every hold out an arrival is dropped, not copied, and goes straight back. A
    /// hold the encoder dropped is requeued first, so the next arrival takes its place.
    #[test]
    fn an_arrival_spends_only_a_hold_that_came_back() {
        let [a, b, c, d, e] = dmabufs::<5>();
        let mut rig = Rig::raw_lane(&[a, b, c]);
        let mut encoding = Vec::new();
        for buf in [a, b, c] {
            rig.process(&[buf]);
            encoding.push(rig.take().expect("a frame"));
        }
        rig.process(&[d]);
        assert!(rig.take().is_none(), "no hold, no frame");
        assert_eq!((rig.back(), rig.ud.held_drops), (vec![d], 1));
        // The encoder lets `a` go; the loop has not serviced that wake when `e` arrives.
        encoding.remove(0);
        rig.process(&[e]);
        assert_eq!(
            rig.back(),
            [a],
            "the dropped hold's buffer, not the arrival"
        );
        assert!(rig.take().is_some());
        assert_eq!(rig.ud.held_drops, 1);
    }

    /// A burst keeps its newest buffer and hands each older one back once, in order.
    /// One of them the book still lists was re-sent while held: its stale hold then
    /// releases nothing.
    #[test]
    fn a_burst_hands_its_stale_buffers_back_once() {
        let pool = dmabufs::<4>();
        let [a, b, c, _] = pool;
        let mut rig = Rig::raw_lane(&pool);
        rig.process(&[a]);
        let held = rig.take().expect("a frame");
        rig.process(&[a, b, c]);
        assert_eq!(rig.back(), [a, b]);
        assert_eq!(rig.ud.shared.resent.load(Relaxed), 1);
        drop(held);
        assert_eq!(rig.service(), 0, "the purged hold releases nothing");
        assert!(rig.back().is_empty());
        drop(rig.take().expect("the newest buffer's frame"));
        assert_eq!(rig.service(), 1);
        assert_eq!(rig.back(), [c]);
    }

    /// PipeWire below 1.6: the producer sends a buffer again while its hold is out. The
    /// new frame re-holds it, the stale hold releases nothing, and it rejoins once.
    #[test]
    fn a_resent_buffer_is_reheld_under_the_new_frame() {
        let pool = dmabufs::<4>();
        let mut rig = Rig::raw_lane(&pool);
        rig.process(&pool[..1]);
        let stale = rig.take().expect("a frame");
        rig.process(&pool[..1]);
        let fresh = rig.take().expect("the re-sent buffer's frame");
        assert_eq!(rig.ud.shared.resent.load(Relaxed), 1);
        drop(stale);
        assert_eq!(rig.service(), 0);
        assert!(rig.back().is_empty(), "the new frame still reads it");
        drop(fresh);
        assert_eq!(rig.service(), 1);
        assert_eq!(rig.back(), [pool[0]]);
    }

    /// The stale hold drops before the re-send arrives, and the loop has not run its wake.
    /// The arrival must not hand back the buffer it is about to publish.
    #[test]
    fn a_resent_buffer_is_not_handed_back_by_its_stale_release() {
        let pool = dmabufs::<4>();
        let mut rig = Rig::raw_lane(&pool);
        rig.process(&pool[..1]);
        drop(rig.take().expect("a frame"));
        rig.process(&pool[..1]);
        assert!(rig.back().is_empty(), "the new frame still reads it");
        let fresh = rig.take().expect("the re-sent buffer's frame");
        assert_eq!(rig.ud.shared.resent.load(Relaxed), 1);
        drop(fresh);
        assert_eq!(rig.service(), 1);
        assert_eq!(rig.back(), [pool[0]]);
    }

    /// A tiled 10-bit frame the raw lane does not take is never de-padded as LINEAR: the
    /// capture refuses the tiled offer and rebuilds.
    #[test]
    fn a_tiled_10bit_frame_without_the_raw_lane_rebuilds() {
        let pool = dmabufs::<4>();
        let mut rig = Rig::new();
        rig.ud.importer.present.store(true, Relaxed);
        rig.negotiate(VideoFormat::xBGR_210LE, (16, 4), 0x0200_0000_0000_0001);
        rig.allocate(&pool);
        rig.process(&pool[..1]);
        assert!(rig.take().is_none(), "no scrambled frame");
        assert_eq!(rig.ud.shared.status(), StreamStatus::Broken);
        assert!(rig.ud.shared.health.hdr_tiled_refused());
    }

    /// Renegotiation frees the pool under its holds, and the next pool reuses the
    /// addresses. A hold of the old pool releases nothing: not while its buffer is
    /// freed, and not once the address has a new tenant.
    #[test]
    fn a_renegotiated_pool_takes_no_release_from_the_old_one() {
        let pool = dmabufs::<4>();
        let [a, b, ..] = pool;
        let mut rig = Rig::raw_lane(&pool);
        rig.process(&[a]);
        let old_a = rig.take().expect("a frame");
        rig.process(&[b]);
        let old_b = rig.take().expect("a frame");
        for &buf in &pool {
            on_remove_buffer(&mut rig.ud, buf as usize);
        }
        drop(old_a);
        assert_eq!(rig.service(), 0, "its buffer is freed");
        rig.negotiate(VideoFormat::BGRx, (16, 4), 0);
        rig.allocate(&pool);
        rig.process(&[b]);
        let new_b = rig.take().expect("the new pool's frame");
        drop(old_b);
        assert_eq!(rig.service(), 0, "the address has a new tenant");
        assert!(rig.back().is_empty());
        let resent = rig.ud.shared.resent.load(Relaxed);
        assert_eq!(resent, 0, "a buffer of the new pool, not a re-send");
        drop(new_b);
        assert_eq!(rig.service(), 1);
        assert_eq!(rig.back(), [b]);
        assert_eq!((rig.ud.pool.live, rig.ud.pool.high_water), (4, 4));
    }

    /// A new Format pod is what the next buffer is read by: its size and its layout.
    #[test]
    fn a_new_format_is_what_the_next_buffer_is_read_by() {
        let mut rig = Rig::new();
        rig.negotiate(VideoFormat::BGRx, (2, 2), 0);
        // Two rows of 8 bytes at a 12-byte stride.
        let padded = shm(&pixels(24), 12);
        rig.process(&[padded]);
        let frame = rig.take().expect("a frame");
        let FramePayload::Cpu(tight) = &frame.payload else {
            panic!("a CPU frame");
        };
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!(frame.format, PixelFormat::Bgrx);
        assert_eq!(tight[..8], pixels(8)[..]);
        assert_eq!(tight[8..], pixels(20)[12..]);

        rig.negotiate(VideoFormat::RGB, (2, 1), 0);
        assert_eq!(
            rig.ud.shared.negotiation().fixated.map(|f| f.size),
            Some((2, 1))
        );
        rig.process(&[padded]);
        let frame = rig.take().expect("a frame");
        let FramePayload::Cpu(tight) = &frame.payload else {
            panic!("a CPU frame");
        };
        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(frame.format, PixelFormat::Rgb);
        assert_eq!(tight[..], pixels(6)[..]);
        assert_eq!(rig.back(), [padded, padded], "a CPU frame holds no buffer");
    }

    /// gamescope offers its 10-bit formats under BT.709 too: the transfer function the
    /// producer fixated decides, not the bit depth.
    #[test]
    fn a_ten_bit_format_is_hdr_only_under_pq() {
        let mut rig = Rig::new();
        let mut fixate = |transfer| {
            let pod = format_pod(VideoFormat::xBGR_210LE, (2, 2), Some(0), Some(transfer));
            let id = ParamType::Format.as_raw();
            on_param_changed(&mut rig.ud, id, Pod::from_bytes(&pod));
            rig.ud.shared.negotiation().fixated.is_some_and(|f| f.hdr)
        };
        assert!(fixate(SPA_VIDEO_TRANSFER_SMPTE2084));
        assert!(!fixate(spa_sys::SPA_VIDEO_TRANSFER_BT709));
        assert_eq!(rig.ud.format, Some(PixelFormat::X2Bgr10));
    }

    /// PipeWire's fallback to memory buffers renegotiates with a pod that names neither a
    /// modifier nor a transfer function. Neither carries over from the format before.
    #[test]
    fn a_pod_inherits_nothing_it_leaves_out() {
        let mut rig = Rig::new();
        let mut fixate = |modifier, transfer| {
            let pod = format_pod(VideoFormat::xBGR_210LE, (2, 2), modifier, transfer);
            let id = ParamType::Format.as_raw();
            on_param_changed(&mut rig.ud, id, Pod::from_bytes(&pod));
            let hdr = rig.ud.shared.negotiation().fixated.is_some_and(|f| f.hdr);
            (rig.ud.modifier, rig.ud.info.transfer_function(), hdr)
        };
        let pq = SPA_VIDEO_TRANSFER_SMPTE2084;
        assert_eq!(fixate(Some(7), Some(pq)), (7, pq, true));
        assert_eq!(fixate(None, None), (0, 0, false));
    }

    /// KWin's birth mode: buffers in the doomed size go straight back until the producer
    /// renegotiates to the expected one.
    #[test]
    fn the_birth_gate_holds_frames_until_the_expected_mode() {
        let mut rig = Rig::new();
        rig.ud.expect_dims = Some((4, 2));
        rig.negotiate(VideoFormat::BGRx, (2, 2), 0);
        let doomed = shm(&pixels(16), 8);
        rig.process(&[doomed]);
        assert!(rig.take().is_none());
        assert_eq!(rig.back(), [doomed]);
        rig.negotiate(VideoFormat::BGRx, (4, 2), 0);
        let real = shm(&pixels(32), 16);
        rig.process(&[real]);
        let frame = rig.take().expect("a frame in the expected mode");
        assert_eq!((frame.width, frame.height), (4, 2));
        assert_eq!(rig.ud.expect_dims, None);
    }

    /// A zero-copy session handed memory instead of a dmabuf takes the CPU copy.
    #[test]
    fn passthrough_copies_a_buffer_that_is_no_dmabuf() {
        let mut rig = Rig::new();
        rig.ud.vaapi_passthrough = true;
        rig.negotiate(VideoFormat::BGRx, (2, 2), 0);
        let memory = shm(&pixels(24), 12);
        rig.process(&[memory]);
        let frame = rig.take().expect("the CPU copy");
        assert!(matches!(&frame.payload, FramePayload::Cpu(p) if p.len() == 16));
        assert_eq!(rig.ud.passthrough_fallbacks.frames, 1);
        assert_eq!(rig.back(), [memory]);
        assert_ne!(rig.ud.shared.status(), StreamStatus::Broken);
    }

    /// Tiled bytes read as linear are a scrambled picture, so a tiled session has no
    /// CPU copy to take: it refuses the tiled offer and asks for a rebuild.
    #[test]
    fn a_tiled_session_handed_memory_rebuilds() {
        let mut rig = Rig::new();
        rig.ud.vaapi_passthrough = true;
        rig.negotiate(VideoFormat::BGRx, (2, 2), 1);
        let memory = shm(&pixels(24), 12);
        for _ in 0..2 {
            rig.process(&[memory]);
            assert!(rig.take().is_none());
            assert_eq!(rig.back(), [memory]);
        }
        assert_eq!(rig.ud.shared.status(), StreamStatus::Broken);
        assert!(rig.ud.shared.health.passthrough_tiled_refused());
        let read = rig.ud.passthrough_fallbacks.frames;
        assert_eq!(read, 1, "a broken capture reads no more buffers");
    }

    /// A pool too shallow to lend a buffer: the dmabuf is copied, never published raw.
    #[test]
    fn a_pool_that_cannot_hold_takes_the_cpu_copy() {
        let pool = dmabufs::<2>();
        let mut rig = Rig::raw_lane(&pool);
        rig.process(&pool[..1]);
        let frame = rig.take().expect("the CPU copy");
        let FramePayload::Cpu(tight) = &frame.payload else {
            panic!("a CPU frame");
        };
        assert_eq!(tight[..], pixels(256)[..]);
        assert_eq!(rig.back(), [pool[0]]);
        assert_eq!(rig.ud.held_drops, 0);
    }

    /// What publishes nothing still goes back to the producer, once.
    #[test]
    fn a_buffer_that_publishes_nothing_goes_straight_back() {
        let negotiated = |format| {
            let mut rig = Rig::new();
            rig.negotiate(format, (2, 2), 0);
            rig
        };
        let good = shm(&pixels(16), 8);
        let corrupted = spa_sys::SPA_META_HEADER_FLAG_CORRUPTED;
        let corrupted = buffer(memory(&pixels(16)), chunk(16, 8), corrupted);
        let empty = buffer(memory(&pixels(16)), chunk(0, 8), 0);
        let idle = negotiated(VideoFormat::BGRx);
        idle.ud.shared.active.store(false, Relaxed);
        // Only a Format pod negotiates.
        let mut unnegotiated = Rig::new();
        let pod = format_pod(VideoFormat::BGRx, (2, 2), Some(0), None);
        let buffers = ParamType::Buffers.as_raw();
        on_param_changed(&mut unnegotiated.ud, buffers, Pod::from_bytes(&pod));
        on_param_changed(&mut unnegotiated.ud, ParamType::Format.as_raw(), None);
        assert_eq!(unnegotiated.ud.shared.negotiation().fixated, None);

        for (what, mut rig, buf) in [
            ("corrupted", negotiated(VideoFormat::BGRx), corrupted),
            (
                "an empty memory chunk",
                negotiated(VideoFormat::BGRx),
                empty,
            ),
            (
                "shorter than its frame",
                negotiated(VideoFormat::BGRx),
                shm(&pixels(8), 8),
            ),
            (
                "a layout no encoder reads",
                negotiated(VideoFormat::I420),
                good,
            ),
            ("not negotiated", unnegotiated, good),
            ("between sessions", idle, good),
        ] {
            rig.process(&[buf]);
            assert!(rig.take().is_none(), "{what}");
            assert_eq!(rig.back(), [buf], "{what}");
        }
    }
}
