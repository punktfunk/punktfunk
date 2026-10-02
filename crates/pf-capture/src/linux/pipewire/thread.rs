//! The PipeWire loop thread: connect, offer, negotiate, then run `.process` until quit.

use super::consume::{consume_frame, realtime_minus_monotonic_ns};
use super::hold::{pool_ask, DeferredRequeue, HoldBook, PoolCensus};
use super::offers::{hdr_modifier_offers, offer_pacing, packed_modifier_offers, probe_producer};
use super::pacer::{wire_interval, Pacer, RawTimer, RequestListener, HEARTBEAT};
use super::plan::{
    consumer_kind, resolved_capture_arm, ImportState, NegotiationPlan, PassthroughFallbacks,
};
use super::{map_format, UserData};
use crate::linux::pw_cursor::{update_cursor_meta, CursorState};
use crate::linux::pw_pods::{
    build_cursor_meta_param, build_damage_meta_param, build_default_format_obj,
    build_dmabuf_buffers, build_dmabuf_format, build_hdr_dmabuf_format, build_header_meta_param,
    build_mappable_buffers, build_sdr10_dmabuf_format, build_shm_only_buffers,
    build_sync_timeline_meta_param, serialize_pod, video_raw, Extent, Pacing, HDR_FORMAT_ORDER,
    SPA_VIDEO_TRANSFER_SMPTE2084,
};
use crate::linux::sync_timeline::{hand_back, SyncDevice};
use crate::linux::{CaptureOpts, CaptureSignals};
use crate::ZeroCopyPolicy;
use anyhow::{Context, Result};
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::Pod;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;

/// How often the loop looks for buffers that never arrived. A lost one costs the producer a
/// pool slot until then, not a frame.
const UNDELIVERED_SWEEP: std::time::Duration = std::time::Duration::from_millis(100);

/// The PipeWire loop thread for one capture session: connects, resolves the [`Offer`],
/// negotiates, and runs `.process` until `quit_rx`, `broken`, or disconnect.
#[allow(clippy::too_many_arguments)]
pub(in crate::linux) fn pipewire_thread(
    fd: Option<OwnedFd>,
    node_id: u32,
    // Arrivals awaiting their renders. A consumer that falls behind loses the oldest.
    queue: super::FrameQueue,
    wake: SyncSender<()>,
    signals: CaptureSignals,
    // Zero-copy decision, resolved once by `spawn_pipewire` — never re-derived here.
    plan: NegotiationPlan,
    // `want_444`/`want_hdr` pick the pod family; `expect_exact_dims` arms the birth-mode gate.
    opts: CaptureOpts,
    preferred: Option<(u32, u32, u32)>,
    quit_rx: pw::channel::Receiver<()>,
    // Encode-backend facts from the facade — never re-derived here.
    policy: ZeroCopyPolicy,
) -> Result<()> {
    crate::pwinit::ensure_init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    // Capturer `Drop` lands here on the loop thread and stops `run()` so the thread unwinds
    // instead of blocking to process exit. Hold the attachment for the loop's life. The
    // registry probe below also runs the loop; `quit_seen` keeps a quit during it terminal.
    let quit_seen = std::rc::Rc::new(std::cell::Cell::new(false));
    let quit_loop = mainloop.clone();
    let _quit_attach = quit_rx.attach(mainloop.loop_(), {
        let quit_seen = quit_seen.clone();
        move |()| {
            tracing::debug!("pipewire: quit signal received — stopping capture loop");
            quit_seen.set(true);
            quit_loop.quit();
        }
    });
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    // Portal source: fd to a sandboxed PipeWire remote. KWin virtual-output: no fd, default daemon.
    let core = match fd {
        Some(fd) => context
            .connect_fd_rc(fd, None)
            .context("pw connect_fd (portal remote)")?,
        None => context
            .connect_rc(None)
            .context("pw connect (default daemon)")?,
    };
    // Lazy driver (PipeWire ≥ 1.2.7 "headless server" scheduling): the producer paints only
    // in a graph cycle this stream starts, so the encode loop owns the tick and no second
    // clock beats against it. Only a producer that emits RequestProcess (Mutter ≥ 49 virtual
    // monitors) may be driven this way; any other stays the driver as before.
    let probe = probe_producer(&core, &mainloop, node_id);
    let lazy = opts.lazy && probe.supports_request;
    if quit_seen.get() {
        return Ok(());
    }

    let mut offer = resolve_offer(&plan, &policy, &signals.health, &opts);
    // Latch must fire only for an offer actually made — `plan.build_importer` cannot know
    // the importer constructed.
    signals.gpu_dmabuf_offer.store(
        offer.want_dmabuf && !plan.vaapi_passthrough && !opts.want_hdr,
        Ordering::Relaxed,
    );
    log_resolved_arm(&offer, &plan, &policy, &signals.health, &opts);

    // Holds on published frames park their release from whichever thread drops last and wake
    // this channel; a withheld buffer rejoins only on the loop thread — the receiver (attached
    // after the stream exists) or `try_defer` drains the parked releases.
    let (requeue_tx, requeue_rx) = pw::channel::channel::<()>();
    // Explicit sync needs a dmabuf lane and a DRM node that serves syncobjs; whether a buffer
    // then carries sync points is the producer's call at negotiation.
    let sync = (crate::explicit_sync() && (opts.want_hdr || offer.want_dmabuf))
        .then(SyncDevice::open)
        .flatten()
        .map(std::sync::Arc::new);
    let defer = std::sync::Arc::new(DeferredRequeue {
        book: std::sync::Mutex::new(HoldBook::default()),
        pending: std::sync::Mutex::new(Vec::new()),
        wake: requeue_tx,
        logged_active: std::sync::atomic::AtomicBool::new(false),
        logged_shallow: std::sync::atomic::AtomicBool::new(false),
        sync: sync.clone(),
        pool: Default::default(),
        undelivered: Default::default(),
    });

    // The heartbeat timer reads `driving` after `signals` moves into the listener's state.
    let signals_hb = signals.clone();
    // A driven producer paints only in cycles this stream starts; the pacer starts one per
    // request, no sooner than a wire interval after the last. Every entry point runs on this
    // thread. The stream pointer lands once the stream exists.
    let pacer = lazy.then(|| Pacer::new(wire_interval(preferred)));
    // Shared with the consumer, which imports held frames at its own tick.
    let importer = offer.importer.take();
    signals
        .has_importer
        .store(importer.is_some(), Ordering::Relaxed);
    *signals.importer.lock().unwrap_or_else(|e| e.into_inner()) = importer;
    let signals_exit = signals.clone();
    let hdr_tiled_raw = (opts.want_hdr || opts.sdr10_native)
        && policy.gamescope_tiled
        && plan.nvenc_raw
        && !signals.health.hdr_tiled_refused();
    let data = UserData {
        info: VideoInfoRaw::default(),
        format: None,
        modifier: 0,
        queue,
        wake,
        signals,
        vaapi_passthrough: plan.vaapi_passthrough,
        // Same predicate `hdr_modifier_offers` used for the NVENC raw lane.
        hdr_tiled_raw,
        import_policy: plan.import_policy.for_ten_bit_sdr(opts.ten_bit_sdr),
        import_state: ImportState::default(),
        dbg_log_n: 0,
        pts: crate::pts_provenance::PtsProvenance::new(),
        pts_reported: std::time::Instant::now(),
        rt_minus_mono_ns: realtime_minus_monotonic_ns(),
        hdr_pts_enabled: pf_host_config::env_on("PUNKTFUNK_CAPTURE_HDR_PTS").unwrap_or(true),
        pool: PoolCensus::default(),
        passthrough_fallbacks: PassthroughFallbacks::default(),
        cursor: CursorState::new(opts.cursor_id0_hides),
        expect_dims: if opts.expect_exact_dims {
            preferred.map(|(w, h, _)| (w, h))
        } else {
            None
        },
        gate_skips: 0,
        gate_since: None,
        defer: defer.clone(),
        pacer: pacer.clone(),
        held_drops: 0,
        damage: DamageGate::new(wire_interval(preferred)),
        undamaged: 0,
        sync: sync.clone(),
    };

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
    let stream =
        pw::stream::StreamBox::new(&core, "punktfunk-screencast", props).context("pw Stream")?;
    if let Some(p) = &pacer {
        p.stream.set(stream.as_raw_ptr());
    }

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(on_state_changed)
        .param_changed(on_param_changed)
        // Pool census. `remove_buffer` also purges the deferred-requeue book: the buffer is
        // being freed under any hold, so that hold's later release must be a no-op (generation
        // in `HoldBook::complete` also covers the address being reused by a new pool).
        .add_buffer(|_stream, ud, buf| {
            ud.pool.add();
            if let Ok(mut pool) = ud.defer.pool.lock() {
                pool.push(buf as usize);
            }
        })
        .remove_buffer(|_stream, ud, buf| {
            ud.pool.remove();
            if let Ok(mut pool) = ud.defer.pool.lock() {
                pool.retain(|&b| b != buf as usize);
            }
            if let Some(dev) = &ud.sync {
                dev.forget(buf as usize);
            }
            if let Ok(mut book) = ud.defer.book.lock() {
                book.purge(buf as usize);
            }
        })
        .process(on_process)
        .register()
        .context("register stream listener")?;

    // A `BufferHold` dropping on any thread only parks and wakes; this loop-thread callback
    // (or `try_defer`, whichever runs first) is where a withheld buffer rejoins.
    let defer_cb = defer.clone();
    let stream_ptr = stream.as_raw_ptr() as usize;
    let _requeue_attach = requeue_rx.attach(mainloop.loop_(), move |()| {
        // SAFETY: the loop thread dispatches this. The stream outlives this attached receiver
        // (declared after it, dropped before it), and the loop stops dispatching once `run()`
        // returns.
        unsafe { defer_cb.drain(stream_ptr as *mut pw::sys::pw_stream) };
    });
    // On a timer, not from `.process`: a producer whose whole pool went undelivered sends
    // nothing more, and `.process` never runs again.
    let _undelivered = sync.is_some().then(|| {
        let defer = defer.clone();
        // SAFETY: the loop thread dispatches this between callbacks, and the pool lists only
        // buffers the stream still has.
        let timer = mainloop
            .loop_()
            .add_timer(move |_| unsafe { defer.recover_undelivered() });
        let _ = timer.update_timer(Some(UNDELIVERED_SWEEP), Some(UNDELIVERED_SWEEP));
        timer
    });

    // `PUNKTFUNK_PW_FIXED_POD="WxH"`: one fixed format, to bisect against a producer's EnumFormat.
    let fixed_pod: Option<(u32, u32)> = std::env::var("PUNKTFUNK_PW_FIXED_POD")
        .ok()
        .and_then(|v| v.split_once('x').map(|(w, h)| (w.parse(), h.parse())))
        .and_then(|(w, h)| Some((w.ok()?, h.ok()?)));
    if let Some((fw, fh)) = fixed_pod {
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
    let pods = build_params(
        &offer,
        &plan,
        &opts,
        preferred,
        probe.framerate_mhz,
        sync.is_some(),
        fixed_pod,
    )?;
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
        .context("pw stream connect")?;

    let _pacing = pacer
        .as_ref()
        .map(|p| install_pacer(mainloop.loop_(), &stream, p, signals_hb));

    // Blocks until capturer `Drop` fires the quit channel. The importer goes here, not with
    // the last `CaptureSignals` clone: the next pipeline must find the EGL/CUDA state gone.
    mainloop.run();
    signals_exit.has_importer.store(false, Ordering::Relaxed);
    *signals_exit
        .importer
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    Ok(())
}

/// What this pipeline offers the producer, resolved once from the plan, the policy and whether
/// the importer constructed.
struct Offer {
    /// The EGL→CUDA importer, handed to the consumer once the offer is logged.
    importer: Option<pf_zerocopy::Importer>,
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

/// Build the importer where the plan wants one, then the modifier lists it and the policy
/// allow. The isolated worker (design/zerocopy-worker-isolation.md) turns a driver fault into a
/// dead worker, not a dead host; a construction failure is the CPU path.
fn resolve_offer(
    plan: &NegotiationPlan,
    policy: &ZeroCopyPolicy,
    health: &pf_zerocopy::ZeroCopyHealth,
    opts: &CaptureOpts,
) -> Offer {
    if plan.gpu_import_latched {
        tracing::warn!(
            "zero-copy GPU import disabled for this capture identity (repeated import-worker \
             deaths or a previous dmabuf negotiation timeout) — using CPU path"
        );
    }
    let mut importer = if plan.build_importer {
        match pf_zerocopy::Importer::new_for_capture() {
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
    // Per-fourcc offers: importer lists plus the encoder-proved gamescope seed
    // and the PyroWave Vulkan list, finalized by `dmabuf_modifiers_for_producer`.
    let (modifiers, modifiers_bgra, extend_pyrowave) = packed_modifier_offers(
        policy,
        health,
        importer.as_mut(),
        plan.vaapi_passthrough,
        opts.producer_is_gamescope,
    );
    let hdr_modifiers = hdr_modifier_offers(
        policy,
        health,
        importer.as_mut(),
        opts.want_hdr || opts.sdr10_native,
        plan.vaapi_passthrough,
        opts.producer_is_gamescope,
        plan.nvenc_raw,
    );
    if extend_pyrowave {
        tracing::info!(
            count = modifiers.len(),
            "zero-copy: advertising the PyroWave device's Vulkan-importable dmabuf modifiers"
        );
    }
    let want_dmabuf = plan.want_dmabuf(importer.is_some(), &modifiers);
    Offer {
        importer,
        modifiers,
        modifiers_bgra,
        hdr_modifiers,
        extend_pyrowave,
        want_dmabuf,
    }
}

/// One line for the resolved arm and its consumer, then the line that explains the arm. The
/// detail lines explain an arm; they do not state which one this session took.
fn log_resolved_arm(
    offer: &Offer,
    plan: &NegotiationPlan,
    policy: &ZeroCopyPolicy,
    health: &pf_zerocopy::ZeroCopyHealth,
    opts: &CaptureOpts,
) {
    let (modifiers, want_dmabuf) = (&offer.modifiers, offer.want_dmabuf);
    let vaapi_passthrough = plan.vaapi_passthrough;
    let consumer = consumer_kind(
        policy.pyrowave_session,
        policy.backend_is_vaapi,
        policy.backend_is_gpu,
    );
    let arm = resolved_capture_arm(plan, offer.importer.is_some(), want_dmabuf);
    tracing::info!(
        capture_arm = arm.as_str(),
        consumer = consumer.as_str(),
        modifier_count = if opts.want_hdr || opts.sdr10_native {
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
        raw_dmabuf_latch = health.raw_state(),
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
            if std::env::var_os("PUNKTFUNK_ZEROCOPY").is_some() {
                "PUNKTFUNK_ZEROCOPY is set falsy"
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
            "4:4:4 zero-copy: tiled dmabufs convert to planar YUV444 (BT.709) on the GPU — \
             NVENC fed native full-chroma YUV, no CPU pixel path"
        );
    } else if want_dmabuf && !vaapi_passthrough && pf_zerocopy::nv12_enabled() {
        tracing::info!(
            "PUNKTFUNK_NV12: tiled dmabufs convert to NV12 (BT.709 limited) on the GPU — NVENC \
             fed native YUV (no internal RGB→YUV CSC)"
        );
    }
}

/// Every param the stream connects with, in order: the format pods, the Buffers pods (the
/// explicit-sync twin that demands the meta ahead of the plain one), then the metas. Pure:
/// every environment read is already in the arguments.
///
/// Zero-copy offers only dmabuf formats with modifiers we can import (offering SHM makes the
/// compositor pick SHM). Modifiers go out as MANDATORY `ChoiceEnum::Enum`; this is not the
/// two-step DONT_FIXATE handshake (`ChoiceFlags` cannot express it). gamescope paints the Steam
/// overlay into this node only when the negotiated `gamescope_focus_appid` is 0, the default:
/// never advertise a non-zero one (the Remote-Play branch, which drops the overlay).
fn build_params(
    offer: &Offer,
    plan: &NegotiationPlan,
    opts: &CaptureOpts,
    preferred: Option<(u32, u32, u32)>,
    producer_mhz: bool,
    sync: bool,
    fixed_pod: Option<(u32, u32)>,
) -> Result<Vec<Vec<u8>>> {
    let format_pods = |unpaced: bool| -> Result<Vec<Vec<u8>>> {
        let pacing = offer_pacing(unpaced, producer_mhz, opts.producer_is_gamescope, preferred);
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
                )?);
            }
            for (fmt, list) in &offer.hdr_modifiers {
                pods.push(build_hdr_dmabuf_format(*fmt, list, preferred, pacing)?);
            }
            return Ok(pods);
        }
        if !offer.want_dmabuf {
            // The fixed bisect pod stays exactly what the operator typed.
            let o = match fixed_pod {
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
                )?);
            }
            for (fmt, list) in &offer.hdr_modifiers {
                pods.push(build_sdr10_dmabuf_format(*fmt, list, preferred, pacing)?);
            }
        }
        if plan.prefer_native_nv12 {
            // First compatible consumer pod wins. Pinning BT.709 limited selects gamescope's
            // RGB→NV12 shader with our bitstream colorimetry.
            pods.push(build_dmabuf_format(
                VideoFormat::NV12,
                &[0],
                preferred,
                pacing,
            )?);
        }
        if !offer.modifiers.is_empty() {
            pods.push(build_dmabuf_format(
                VideoFormat::BGRx,
                &offer.modifiers,
                preferred,
                pacing,
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
    ud.signals.streaming.store(streaming, Ordering::Relaxed);
    ud.signals.driving.store(driving, Ordering::Relaxed);
    if matches!(new, pw::stream::StreamState::Error(_)) {
        ud.signals.errored.store(true, Ordering::Relaxed);
    }
    if let Some(p) = &ud.pacer {
        p.on_streaming(driving);
    }
}

fn on_param_changed(_stream: &pw::stream::Stream, ud: &mut UserData, id: u32, param: Option<&Pod>) {
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
    // Parse once (`parse` takes `&mut self`) and report failure. On `Err`, `negotiated`
    // stays false so the timeout looks like "no accepted format" — a malformed pod we
    // accepted, not a format mismatch.
    let parsed = ud.info.parse(param);
    if let Err(e) = &parsed {
        tracing::error!(
            error = %e,
            "pipewire: the negotiated Format pod does not parse — capture will time out \
             with no usable format"
        );
    }
    if parsed.is_err() {
        return;
    }
    ud.signals.negotiated.store(true, Ordering::Relaxed);
    // Renegotiation replaces the pool: cached per-buffer imports key on buffers
    // that no longer exist, and a recycled fd/inode must not resolve to a stale import.
    if let Some(imp) = ud
        .signals
        .importer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        imp.clear_cache();
    }
    let sz = ud.info.size();
    // Gamescope cursor source scales root→frame (`xfixes_cursor::scale_to_frame`).
    ud.signals.frame_size.store(
        (u64::from(sz.width) << 32) | u64::from(sz.height),
        Ordering::Relaxed,
    );
    ud.format = map_format(ud.info.format());
    ud.modifier = ud.info.modifier();
    // A 10-bit format is HDR only when the producer fixated PQ on it: gamescope offers its
    // 10-bit formats under BT.709 too (`sdr10_native`).
    let hdr = ud.format.is_some_and(|f| f.is_ten_bit())
        && ud.info.transfer_function() == SPA_VIDEO_TRANSFER_SMPTE2084;
    ud.signals.hdr_negotiated.store(hdr, Ordering::Relaxed);
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

/// `.process`: latest frame only. The newest buffer passes the birth gate, then a stale-frame
/// skip, then [`consume_frame`]. It is requeued exactly once, unless a hold took it: then
/// [`super::hold::BufferHold`] owns the requeue, and doing both hands the producer the buffer
/// twice. Dequeue and requeue stay outside `catch_unwind`, where a panic would strand the
/// buffer and shrink the fixed pool.
fn on_process(stream: &pw::stream::Stream, ud: &mut UserData) {
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
        unsafe { ud.requeue_unpublished(stream.as_raw_ptr(), newest) };
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
            if let Ok(mut slot) = ud.signals.cursor_live.lock() {
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
        consume_frame(ud, spa_buf, newest, stream.as_raw_ptr(), header.pts);
    }));
    // `newest`'s book entry is stable here: only this thread removes entries, never
    // `newest`'s inside `.process`. A panic after publish still leaves the hold live.
    let withheld = ud
        .defer
        .book
        .lock()
        .map(|b| b.contains(newest as usize))
        .unwrap_or(false);
    if !withheld {
        // SAFETY: all reads of `spa_buf`/`newest` (update_cursor_meta, consume_frame)
        // completed inside the closure above; `newest` was dequeued from this stream,
        // not yet requeued, and — per the `withheld` check — carries no hold that would
        // requeue it a second time.
        unsafe { hand_back(ud.sync.as_deref(), stream.as_raw_ptr(), newest) };
    }
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
    stream: &pw::stream::Stream,
    ud: &mut UserData,
) -> Option<(*mut pw::sys::pw_buffer, u32)> {
    // SAFETY: `stream` is the live stream PipeWire passes into this `.process` callback on the
    // loop thread; `dequeue_raw_buffer` returns a stream-owned `*mut pw_buffer` or null
    // (null-checked), single-threaded so no concurrent access.
    let mut newest = unsafe { stream.dequeue_raw_buffer() };
    if newest.is_null() {
        return None;
    }
    let mut drained = 1u32;
    loop {
        // SAFETY: same stream/loop-thread contract; returns the next stream-owned buffer or null.
        let next = unsafe { stream.dequeue_raw_buffer() };
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
        unsafe { ud.requeue_unpublished(stream.as_raw_ptr(), newest) };
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
    signals: CaptureSignals,
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
            // Re-read the role: PipeWire may assign the driver after the Streaming edge.
            // SAFETY: the stream outlives this timer source (dropped before it).
            let driving = signals.streaming.load(Ordering::Relaxed)
                && unsafe { pw::sys::pw_stream_is_driving(p.stream.get()) };
            if driving != signals.driving.swap(driving, Ordering::Relaxed) {
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

#[cfg(test)]
mod tests {
    use super::{build_params, CaptureOpts, NegotiationPlan, Offer};
    use crate::linux::pipewire::plan::ImportPolicy;
    use pipewire::spa::param::ParamType;
    use pipewire::spa::pod::{deserialize::PodDeserializer, Value};

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
            expect_exact_dims: false,
            cursor_id0_hides: false,
            producer_is_gamescope: false,
            pool_min: crate::POOL_MIN,
            pool_max: None,
            unpaced: false,
            lazy: false,
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

    fn dmabuf_offer() -> Offer {
        Offer {
            importer: None,
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
        let p = build_params(&dmabuf_offer(), &plan(), &opts(), None, false, true, None).unwrap();
        assert_eq!(
            param_ids(&p),
            [format, format, buffers, buffers, meta, meta, meta, meta],
            "BGRx + BGRA, sync twin + plain Buffers, cursor + sync + header + damage metas"
        );
        let gamescope = CaptureOpts {
            producer_is_gamescope: true,
            ..opts()
        };
        let p = build_params(
            &dmabuf_offer(),
            &plan(),
            &gamescope,
            None,
            false,
            false,
            None,
        );
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
        let p = build_params(&dmabuf_offer(), &plan(), &unpaced, None, false, false, None);
        assert_eq!(param_ids(&p.unwrap()).len(), 4 + 4);
        let cpu = Offer {
            want_dmabuf: false,
            ..dmabuf_offer()
        };
        let p = build_params(&cpu, &plan(), &opts(), None, false, false, None).unwrap();
        assert_eq!(param_ids(&p).len(), 1 + 4);
    }
}
