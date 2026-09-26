//! The event-driven async MediaCodec decode loop (default) + its feeder/dispatch/present helpers.

use ndk::data_space::DataSpace;
use ndk::media::media_codec::{AsyncNotifyCallback, MediaCodec, MediaCodecDirection};
use ndk::native_window::NativeWindow;
use punktfunk_core::client::NativeClient;
use punktfunk_core::error::PunktfunkError;
use punktfunk_core::packet::FLAG_SOF;
use punktfunk_core::reanchor::{GateVerdict, ReanchorGate};
use punktfunk_core::session::Frame;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use super::asc_presenter::{asc_backend_selected, AscBackend};
use super::display::{
    apply_reported_dataspace, color_dataspace, install_render_callback, release_render_callback,
    reported_dataspace, DisplayTracker,
};
use super::latency::{
    note_decoded_pts, note_received_frame, now_realtime_ns, take_flags, take_stamp,
};
use super::presenter::{presenter_disabled_by_sysprop, PresentMeter, PresentPriority, Presenter};
use super::setup::{
    boost_hot_threads, boost_thread_priority, codec_mime, create_codec, hdr_static,
    low_latency_format, try_set_frame_rate,
};
use super::surface_control::{Layer, PresentComplete};
use super::vsync::{now_monotonic_ns, VsyncClock, VsyncShared};
use super::{Backstops, DecodeOptions, FRAME_PARK_CAP, IN_FLIGHT_CAP};
use crate::input_stall::{InputStall, INPUT_STALL_PATIENCE};

/// One decoded output buffer ready to release: its codec buffer index + the pts the codec echoed
/// (from the output callback's `BufferInfo`), used to pair the `decode` HUD stat, and the
/// wall-clock instant the output callback fired — the spec's `decoded` point ("decoder output
/// frame available"), stamped at the callback so the event-channel hop + coalescing wait in the
/// loop never inflates the decode stage.
struct OutputReady {
    index: usize,
    pts_us: u64,
    decoded_ns: i128,
    decoded_mono_ns: i64,
}

/// Events the async decode loop reacts to. The codec's async-notify callbacks (which run on its
/// internal looper thread) push the codec ones; the feeder thread pushes `Au`. Each carries only
/// owned/`Copy` data so the callback closures satisfy the `Send` bound and never touch the codec.
pub(super) enum DecodeEvent {
    /// A received access unit from the feeder, ready to queue into the decoder. The `u32` is the
    /// feeder's [`NativeClient::note_frame_index`] verdict — the forward frame-index gap's WIDTH
    /// (0 = none), so the loop arms the freeze gate with the same signal and pre-credits the
    /// reassembler's later `frames_dropped` climb for the loss (the feeder already fired the RFI
    /// request).
    Au(Frame, u32),
    /// An input buffer slot freed (index) — we can queue an AU into it.
    InputAvailable(usize),
    /// A decoded frame is ready (buffer index + echoed pts + the callback-time `decoded` stamps).
    OutputAvailable {
        index: usize,
        pts_us: u64,
        decoded_ns: i128,
        decoded_mono_ns: i64,
    },
    /// The output format changed — re-check the stream's colour signalling (HDR DataSpace).
    FormatChanged,
    /// A panel vsync (from the [`VsyncClock`] thread) — the presenter's retry/pacing tick.
    Vsync,
    /// An `ASurfaceControl` transaction completed (ASurfaceControl backend only): the real latch
    /// time + the previous buffer's release fence, forwarded from the completion callback (a binder
    /// thread) so the decode loop applies it on its own thread.
    PresentComplete(super::surface_control::PresentComplete),
    /// The codec reported an error; `fatal` when neither recoverable nor transient.
    Error { fatal: bool },
}

/// The decoder bring-up rungs, in order, as `(present backend, low-latency keys)`.
/// The backend is `Some(overlay)` for ASC with that reader-usage profile (see
/// [`AscBackend::create`]'s `overlay` doc), `None` for the SurfaceView presenter. The keys are
/// `Some(aggressive)` for that key profile, `None` for no low-latency key at all. See
/// [`bring_up`] for why these axes, and why in this order.
///
/// Consecutive duplicates are collapsed: the `present_backend` sysprop and the low-latency toggle
/// may each already have shed what a rung was going to shed, and re-running a configuration the
/// codec just refused buys nothing but another failed `start`. The first rung is always exactly
/// what the session asked for, so a device that works is never charged for this ladder.
fn bring_up_rungs(asc_wanted: bool, low_latency: bool) -> Vec<(Option<bool>, Option<bool>)> {
    let mut rungs = vec![
        (asc_wanted.then_some(true), Some(low_latency)),
        (asc_wanted.then_some(false), Some(low_latency)),
        (None, Some(low_latency)),
        (None, Some(false)),
        (None, None),
    ];
    rungs.dedup();
    rungs
}

/// Human label for a rung's present backend, for the retry / decoder-started log lines — the
/// string a field log bundle is grepped for, so it names the reader profile, not just "ASC".
fn backend_label(backend: Option<bool>) -> &'static str {
    match backend {
        Some(true) => "ASurfaceControl (overlay reader)",
        Some(false) => "ASurfaceControl (GPU-composited reader)",
        None => "SurfaceView",
    }
}

/// Put `codec` into async-notify mode, forwarding every codec callback onto `ev_tx`.
///
/// Must run BEFORE `configure()`/`start()` so we're async from the first buffer, and once per
/// bring-up rung — a codec that failed `start` is discarded, and its replacement needs its own
/// registration. Each closure only *pushes an event*: no `AMediaCodec` call happens on the codec's
/// looper thread, which is what keeps every buffer op on the decode thread that owns the codec.
///
/// `false` ⇒ the platform refused async mode; that is not something a simpler format or a
/// different output surface can fix, so the caller gives up rather than trying the next rung.
fn install_async_callbacks(codec: &mut MediaCodec, ev_tx: &mpsc::Sender<DecodeEvent>) -> bool {
    let out_tx = ev_tx.clone();
    let in_tx = ev_tx.clone();
    let fmt_tx = ev_tx.clone();
    let err_tx = ev_tx.clone();
    let cb = AsyncNotifyCallback {
        on_input_available: Some(Box::new(move |idx| {
            let _ = in_tx.send(DecodeEvent::InputAvailable(idx));
        })),
        on_output_available: Some(Box::new(move |idx, info| {
            let _ = out_tx.send(DecodeEvent::OutputAvailable {
                index: idx,
                pts_us: info.presentation_time_us().max(0) as u64,
                // The `decoded` HUD point: stamp HERE, on the codec's looper thread, so the
                // decode stage ends when the frame actually became available — not after the
                // channel hop + whatever work the loop coalesces in front of presenting it.
                decoded_ns: now_realtime_ns(),
                // Its monotonic twin, from the same instant. The stats are REALTIME (they
                // fold the host's clock offset in), while the cadence loop and
                // `releaseOutputBufferAtTime` are both CLOCK_MONOTONIC — and the loop is fed
                // and read in one domain, never converted (`punktfunk_core::phase`: a
                // constant offset between domains is what its offset estimator absorbs).
                decoded_mono_ns: now_monotonic_ns(),
            });
        })),
        on_format_changed: Some(Box::new(move |_fmt| {
            let _ = fmt_tx.send(DecodeEvent::FormatChanged);
        })),
        on_error: Some(Box::new(move |e, code, _detail| {
            let fatal = !code.is_recoverable() && !code.is_transient();
            if fatal {
                log::error!("decode: fatal codec error — rebuilding the decoder: {e:?}");
            } else {
                log::warn!("decode: codec error {e:?} (recoverable)");
            }
            let _ = err_tx.send(DecodeEvent::Error { fatal });
        })),
    };
    if let Err(e) = codec.set_async_notify_callback(Some(cb)) {
        log::error!("decode: set_async_notify_callback failed: {e}");
        return false;
    }
    true
}

/// Failed runs in a row whose decoder never presented a frame before the loop gives up. Past
/// this the device keeps refusing the codec, and further rebuilds would only churn it.
const MAX_BARREN_REBUILDS: u32 = 3;

/// How one [`run_codec`] ended.
enum RunEnd {
    /// The session stopped video, or every bring-up rung refused the first codec.
    Stopped,
    /// The codec died or stopped taking input. `presented`: this run showed a frame.
    Failed { presented: bool },
}

/// The decode thread's body (see [`run`]): one [`run_codec`] per codec. A codec that dies or
/// hangs mid-session is torn down and rebuilt on the same surface, so the session keeps its
/// video instead of freezing until the user reconnects.
pub(super) fn run_async(
    client: Arc<NativeClient>,
    window: NativeWindow,
    shutdown: Arc<AtomicBool>,
    stats: Arc<crate::stats::VideoStats>,
    opts: DecodeOptions,
) {
    boost_thread_priority();
    let mut rebuilt = false;
    let mut barren = 0u32;
    // The failed codec's layer, still showing the last good frame until a rebuilt one presents.
    let mut stale: Option<Layer> = None;
    loop {
        match run_codec(
            &client, &window, &shutdown, &stats, &opts, rebuilt, &mut stale,
        ) {
            RunEnd::Stopped => return,
            RunEnd::Failed { presented } => {
                barren = if presented { 0 } else { barren + 1 };
                if barren >= MAX_BARREN_REBUILDS {
                    log::error!(
                        "decode: {barren} rebuilt decoders in a row showed no frame — giving up; \
                         video stays frozen until the stream restarts"
                    );
                    return;
                }
                log::warn!("decode: rebuilding the decoder in place");
                rebuilt = true;
            }
        }
    }
}

/// One codec's life: bring-up, the event loop, teardown. The codec drives the loop: an
/// async-notify callback fires the instant an input buffer frees or a frame finishes decoding,
/// so a decoded frame is presented without waiting out a poll interval. The callbacks run on the
/// codec's looper thread and only push events; every `AMediaCodec` buffer op stays on this
/// thread, which owns the codec. A `pf-decode-feed` thread blocks on the network so this loop
/// never does.
///
/// Each run owns its event channel, so a dead codec's late callbacks cannot hand this run a
/// stale buffer index. `rebuilt`: a previous codec failed, so this one waits for a keyframe.
/// `stale`: a failed run's layer, hidden at this run's first present; a failed run that
/// presented leaves its own layer there.
#[allow(clippy::too_many_arguments)]
fn run_codec(
    client: &Arc<NativeClient>,
    window: &NativeWindow,
    shutdown: &AtomicBool,
    stats: &Arc<crate::stats::VideoStats>,
    opts: &DecodeOptions,
    rebuilt: bool,
    stale: &mut Option<Layer>,
) -> RunEnd {
    let mode = client.mode();
    // The event channel: the callbacks + feeder push, this loop pulls. `Sender` is `Send`, so the
    // callback closures (each capturing a clone) satisfy the async-notify `Send` bound.
    let (ev_tx, ev_rx) = mpsc::channel::<DecodeEvent>();
    let Some((codec, asc)) = bring_up(client, window, opts, &ev_tx, stats) else {
        if !rebuilt {
            return RunEnd::Stopped;
        }
        // The hung codec may still hold the hardware: give its release half a second.
        for _ in 0..50 {
            if shutdown.load(Ordering::Relaxed) {
                return RunEnd::Stopped;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        return RunEnd::Failed { presented: false };
    };
    // The forced TV mode switch (`is_tv` ⇒ ALWAYS strategy) is part of the experimental stack;
    // off, every form factor gets the original soft seamless hint. ASC votes the rate on its own
    // layer instead (the SurfaceView window shows nothing under the ASC path).
    if asc.is_none()
        && mode.refresh_hz > 0
        && !try_set_frame_rate(
            window,
            mode.refresh_hz as f32,
            opts.is_tv && opts.low_latency_mode,
        )
    {
        log::debug!(
            "decode: set_frame_rate({} Hz) unavailable/declined (non-fatal)",
            mode.refresh_hz
        );
    }

    // Skew-corrected latency stats (spec: design/stats-unification.md). Receipt stamps (keyed by the
    // pts we queue) live in a shared map: the feeder writes them at receipt, this loop pairs decoded
    // output back to them. Behind a `Mutex` since two threads touch it — only ever locked while the
    // HUD is visible.
    let clock_offset = client.clock_offset_shared();
    let video_e2e = client.video_e2e_shared();
    let measure_decode = client.wants_decode_latency();
    let in_flight = Arc::new(Mutex::new(VecDeque::<(u64, i128)>::new()));
    // Display stage (spec `display` + the capture→displayed headline): the rendered frame is
    // parked in the tracker at release; the OnFrameRendered callback pairs it with
    // SurfaceFlinger's render timestamp. `render_cb` is the callback's leaked Arc refcount,
    // reclaimed after the codec is dropped below. SurfaceView backend only — the ASC path measures
    // its display stage directly off the transaction completions.
    let meter = Arc::new(PresentMeter::new());
    let tracker = DisplayTracker::new(
        stats.clone(),
        clock_offset.clone(),
        video_e2e.clone(),
        meter.clone(),
    );
    let render_cb = if asc.is_none() {
        install_render_callback(&codec, &tracker)
    } else {
        None
    };
    let priority = PresentPriority::resolve(opts.present_priority, opts.smooth_buffer);
    // The SurfaceView timeline presenter (see `presenter.rs`): newest-wins / smoothing store,
    // one-in-flight glass budget, timeline-timed release. `None` under the ASC backend, or when
    // `debug.punktfunk.presenter = arrival` selects the legacy release-immediately path.
    let presenter = if asc.is_some() {
        None
    } else if presenter_disabled_by_sysprop() {
        log::info!("decode: presenter = arrival (sysprop) — legacy immediate release");
        None
    } else {
        log::info!(
            "decode: presenter = timeline ({})",
            match priority {
                PresentPriority::Latency => "lowest latency".to_string(),
                PresentPriority::Smooth { buffer } => format!("smoothness, buffer {buffer}"),
            }
        );
        Some(Presenter::new(priority, mode.refresh_hz))
    };
    // The vsync clock, started LAZILY on the first decoded frame (see `vsync.rs`); its ticks ride
    // the same event channel. Both presenters need it: the SurfaceView one for its timelines, the
    // ASC one for the panel period (its phase comes from present fences) and the fence poll on
    // every tick.
    let mut vsync: Option<VsyncClock> = None;
    let mut vsync_tx = (presenter.is_some() || asc.is_some()).then(|| ev_tx.clone());
    let ctx = Ctx {
        codec,
        client: client.clone(),
        stats: stats.clone(),
        measure_decode,
        in_flight: in_flight.clone(),
        clock_offset: clock_offset.clone(),
        video_e2e,
        meter,
        tracker,
        window: window.clone(),
        // A persistent Sender for the ASC path: the pump hands it to each transaction's completion
        // callback, and it keeps the event channel alive for those callbacks.
        present_tx: asc.as_ref().map(|_| ev_tx.clone()),
        decoded_size: opts.decoded_size.clone(),
    };
    let mut state = State::new(asc, presenter, ReanchorGate::new(client.frames_dropped()));
    if rebuilt {
        // A fresh decoder holds no reference picture, so every P-frame before a keyframe is a
        // reference error, and reference errors can hang a hardware decoder. None reach this one.
        state.await_keyframe = true;
        state.gate.arm(Instant::now());
        let _ = client.request_keyframe();
    }

    // Feeder thread: block on the network so this loop doesn't (an AU's arrival becomes an event that
    // wakes us immediately, with no input-side poll latency). It also records the `received` HUD stat.
    // `feed_stop` ends it with this run; the session's `shutdown` would end every later run too.
    let feed_stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let client = client.clone();
        let stats = stats.clone();
        let stop = feed_stop.clone();
        std::thread::Builder::new()
            .name("pf-decode-feed".into())
            .spawn(move || {
                feeder_loop(client, stats, measure_decode, in_flight, stop, ev_tx);
            })
            .ok()
    };
    // Only the feeder + callbacks keep the channel alive now (`ev_tx` moved into the feeder).

    // ADPF: same as the sync path — register this thread now, create the session lazily on the first
    // presented frame (by when the pump + audio + feeder threads have registered their tids too).
    client.register_hot_thread();
    let mut hint: Option<crate::adpf::HintSession> = None;
    let mut hint_tried = false;
    // Productive (dispatch+feed+present) time between displayed frames; reported to ADPF once one is
    // presented. The blocking event wait is excluded (idle, not work).
    let mut work_accum_ns: i64 = 0;

    while !shutdown.load(Ordering::Relaxed) && !state.fatal && !state.wedged {
        // Block for the next event (idle wait — excluded from the work tally). The short timeout
        // drives loss-recovery housekeeping when the pipeline is momentarily quiet.
        let ev0 = match ev_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(ev) => Some(ev),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let work_t0 = Instant::now();
        // Coalesce every event already queued into this one work pass — correct newest-only
        // presentation across a decode burst, and batched feeding.
        let mut pass = Pass::default();
        for ev in ev0
            .into_iter()
            .chain(std::iter::from_fn(|| ev_rx.try_recv().ok()))
        {
            state.dispatch(ev, &mut pass);
        }
        let had_output = !state.ready.is_empty();
        let rendered_before = state.rendered;
        state.apply(&ctx, &mut pass);
        state.pump(&ctx, vsync.as_ref().map(|v| v.shared().as_ref()));
        let presented_now = state.rendered > rendered_before;
        // Start the vsync clock LAZILY on the first decoded output (eager, it ticks the panel
        // rate into a session that has no frame yet — the Apple deadline presenter's bootstrap
        // lesson). A `None` from start (no choreographer surface) simply leaves ASAP targets.
        if had_output && vsync.is_none() {
            if let Some(tx) = vsync_tx.take() {
                vsync = VsyncClock::start(
                    opts.panel_hz,
                    Box::new(move || {
                        let _ = tx.send(DecodeEvent::Vsync);
                    }),
                );
                if vsync.is_none() {
                    log::info!("decode: no choreographer clock — presenter uses ASAP targets");
                }
            }
        }

        work_accum_ns += work_t0.elapsed().as_nanos() as i64;
        if presented_now {
            if !hint_tried {
                hint_tried = true;
                hint = start_hint(client, mode.refresh_hz, opts.low_latency_mode);
            }
            if let Some(h) = &hint {
                h.report_actual(work_accum_ns);
            }
            work_accum_ns = 0;
            state.log_progress();
            if let Some(layer) = stale.take() {
                layer.hide();
            }
        }
        state.housekeeping(&ctx, had_output, &pass);
    }

    let State {
        asc,
        presenter,
        fed,
        rendered,
        discarded,
        fatal,
        wedged,
        ..
    } = state;
    if let Some(mut p) = presenter {
        p.release_all(&ctx.codec); // hand every held output buffer back before the codec stops
    }
    let mut asc = asc;
    if let Some(a) = asc.as_mut() {
        a.release_all(); // drop every held image back to the reader pool before it goes away
    }
    drop(vsync); // stop + join the choreographer thread; its channel sends are harmless after
    let _ = ctx.codec.stop();
    // AMediaCodec_delete — after this no render callback can fire. The ASC reader + layer outlive
    // the codec, which rendered into the reader's window. `into_layer` then drops the reader; the
    // layer lives on only while a rebuild needs its last frame, and its control is freed once every
    // in-flight completion callback has also dropped its share.
    drop(ctx);
    let layer = asc.map(AscBackend::into_layer);
    if let Some(ud) = render_cb {
        // SAFETY: the codec was dropped above; this registration's single reclaim.
        unsafe { release_render_callback(ud) };
    }
    // The feeder stops last: while a hung codec is slow to stop, it keeps the receive queue drained.
    feed_stop.store(true, Ordering::SeqCst);
    if let Some(j) = feeder {
        let _ = j.join();
    }
    log::info!("decode: stopped (async, fed={fed} rendered={rendered} discarded={discarded})");
    if (fatal || wedged) && !shutdown.load(Ordering::Relaxed) {
        if rendered > 0 {
            *stale = layer;
        }
        RunEnd::Failed {
            presented: rendered > 0,
        }
    } else {
        RunEnd::Stopped
    }
}

/// Climb the decoder bring-up ladder ([`bring_up_rungs`]) until a codec starts. `None` ⇒ every
/// rung refused, or the platform refused async mode — either way the session has no video.
///
/// `configure()` can succeed and `start()` still fail: start is where the codec negotiates
/// buffers with its output consumer and allocates them, so a decoder that accepted the format
/// can still refuse the surface it has to render into. A codec that failed `start` is in an error
/// state and cannot be reconfigured, so each rung builds a fresh one. The rungs shed what a
/// configure or start can choke on, most-suspect first:
///
///   1. The ASC reader's `COMPOSER_OVERLAY` usage — overlay + GPU-sampled + vendor-vdec in one
///      allocation is what an old OMX-era gralloc refuses (see [`AscBackend::create`]'s `overlay`
///      doc). Usage is the ONLY reader axis worth a rung: `READER_MAX_IMAGES` is not a start-time
///      factor.
///   2. The `AImageReader` entirely — an app-side BufferQueue consumer at all is the residual
///      suspect (a vendor OMX component keying on queues-to-composer).
///   3. The aggressive low-latency key set.
///   4. Every low-latency key. ACodec fails `configure` when an OMX decoder refuses the standard
///      `low-latency` key; Codec2 ignores that key instead.
///
/// The rung that wins is logged: on a device that needs one, that line names the real culprit.
fn bring_up(
    client: &NativeClient,
    window: &NativeWindow,
    opts: &DecodeOptions,
    ev_tx: &mpsc::Sender<DecodeEvent>,
    stats: &crate::stats::VideoStats,
) -> Option<(MediaCodec, Option<AscBackend>)> {
    let mode = client.mode();
    let mime = codec_mime(client.codec);
    // Fetched ONCE, ahead of the ladder, so a retry rung never pays the wait again.
    let hdr_static = hdr_static(client);
    let priority = PresentPriority::resolve(opts.present_priority, opts.smooth_buffer);
    let asc_wanted = asc_backend_selected();
    if !asc_wanted {
        log::info!("decode: present backend = SurfaceView (present_backend sysprop)");
    }
    let rungs = bring_up_rungs(asc_wanted, opts.low_latency_mode);
    for (rung, &(backend, keys)) in rungs.iter().enumerate() {
        if rung > 0 {
            log::warn!(
                "decode: decoder refused that configuration — retrying through {} with {}",
                backend_label(backend),
                match keys {
                    Some(true) => "aggressive low-latency keys ON",
                    Some(false) => "aggressive low-latency keys OFF",
                    None => "no low-latency keys",
                }
            );
        }
        let Some(mut codec) = create_codec(mime, opts.decoder_name.as_deref()) else {
            log::error!("decode: no {mime} decoder on this device");
            return None;
        };
        // The decoder's *actual* resolved name (Kotlin's pick, or the platform default when it
        // fell back) drives both the HUD label and which vendor low-latency keys apply.
        let codec_name = codec.name().unwrap_or_default();
        if rung == 0 {
            stats.set_decoder(&codec_name, opts.ll_feature);
            log::info!(
                "decode: codec mime = {mime}, decoder = {codec_name} (async, low-latency feature: {})",
                opts.ll_feature
            );
        }
        if !install_async_callbacks(&mut codec, ev_tx) {
            return None; // the platform refused async mode outright — no rung changes that
        }
        let format = low_latency_format(mime, &mode, &codec_name, keys, hdr_static.as_ref());
        // The present backend. ASurfaceControl (default) drives its own `AImageReader` output
        // surface + compositor layer, scheduling against the panel's real present clock; the
        // SurfaceView presenter is the fallback for API < 29, an ASC init failure, the
        // `present_backend=surfaceview` sysprop, or a rung that dropped it. A non-null `asc` means
        // the codec renders into the reader, not the SurfaceView window.
        let asc = backend.and_then(|overlay| {
            // The negotiated colour is authoritative (PQ vs HLG, range) — not a guess the codec's
            // output format later corrects; many decoders never echo `color-transfer` at all.
            AscBackend::create(
                window,
                mode.width as i32,
                mode.height as i32,
                opts.surface_size.clone(),
                opts.src_crop.clone(),
                opts.panel_hz,
                color_dataspace(&client.color),
                mode.refresh_hz,
                priority,
                overlay,
            )
            .map(|mut a| {
                a.set_hdr_meta(hdr_static);
                a
            })
        });
        // The decoder's output surface: the reader's window when ASC is active, else the SurfaceView.
        let configure_window: &NativeWindow = asc.as_ref().map_or(window, |a| a.reader_window());
        if let Err(e) = codec.configure(
            &format,
            Some(configure_window),
            MediaCodecDirection::Decoder,
        ) {
            log::error!("decode: configure failed: {e}");
            continue;
        }
        if let Err(e) = codec.start() {
            log::error!("decode: start failed: {e}");
            continue;
        }
        log::info!(
            "decode: decoder started (async) at {}x{} through {} (rung {rung})",
            mode.width,
            mode.height,
            // `asc.as_ref().and(backend)`, not `backend`: an ASC rung whose backend failed to
            // CREATE fell back to the SurfaceView within the rung, and this line must report what
            // actually runs.
            backend_label(asc.as_ref().and(backend))
        );
        return Some((codec, asc));
    }
    // Every rung refused. Say so loudly and in the shape the next reporter can act on: the
    // session stays up (audio/input/library all still work), so without this line the only
    // symptom is a black screen and a keyframe request every 2 s that blames the network.
    log::error!(
        "decode: the {mime} decoder refused EVERY configuration — this session has no video. \
         Audio and input keep working, so the stream will look alive while the screen stays \
         black, and the host will see a keyframe recovery request every 2 s that is this, not \
         a slow link. See the `configure failed` / `start failed` lines above for the reason \
         each rung gave"
    );
    None
}

/// The ADPF hint session, created on the first presented frame. The pump/audio priority boost is
/// part of the experimental low-latency stack; the session itself predates it and always runs
/// (max-performance bias gated inside).
fn start_hint(
    client: &NativeClient,
    refresh_hz: u32,
    low_latency_mode: bool,
) -> Option<crate::adpf::HintSession> {
    let frame_period_ns = if refresh_hz > 0 {
        1_000_000_000i64 / i64::from(refresh_hz)
    } else {
        0
    };
    let tids = client.hot_thread_ids();
    if low_latency_mode {
        boost_hot_threads(&tids);
    }
    let hint = crate::adpf::HintSession::create(frame_period_ns, &tids, low_latency_mode);
    log::info!(
        "decode: ADPF hint session {} — {} hot thread(s), target {frame_period_ns} ns",
        if hint.is_some() {
            "active"
        } else {
            "unavailable"
        },
        tids.len(),
    );
    hint
}

/// What a pass reads and never writes: the codec, the client, the HUD sinks, the receipt map the
/// feeder fills, and the SurfaceView window the codec renders into under that backend.
struct Ctx {
    codec: MediaCodec,
    client: Arc<NativeClient>,
    stats: Arc<crate::stats::VideoStats>,
    /// The adaptive-bitrate controller wants the `decode` stage as its decoder-backlog signal:
    /// then `in_flight` is fed regardless of the HUD.
    measure_decode: bool,
    in_flight: Arc<Mutex<VecDeque<(u64, i128)>>>,
    clock_offset: Arc<AtomicI64>,
    /// The cell the audio plane steers its jitter ring by — the present path is the only point
    /// that knows when a frame actually reached glass; both backends publish into it.
    video_e2e: Arc<AtomicU64>,
    meter: Arc<PresentMeter>,
    tracker: Arc<DisplayTracker>,
    window: NativeWindow,
    /// The persistent Sender each ASC transaction's completion callback rides back on.
    present_tx: Option<mpsc::Sender<DecodeEvent>>,
    /// The session's decoded-size cell, refreshed on each output-format change.
    decoded_size: Arc<AtomicU64>,
}

impl Ctx {
    fn offset(&self) -> i64 {
        self.clock_offset.load(Ordering::Relaxed)
    }
}

/// What one event drain leaves for the rest of the pass.
#[derive(Default)]
struct Pass {
    fmt_dirty: bool,
    vsync_tick: bool,
    /// Parked AUs dropped on overflow this pass — each one is a loss.
    aus_dropped: u64,
    /// The codec freed an input slot this pass (the hung-codec check, see [`InputStall`]).
    input_offered: bool,
    /// ASurfaceControl transaction completions, applied after the drain (on the decode thread,
    /// not the binder thread that posted them).
    present_completes: Vec<PresentComplete>,
}

/// The loop's working state: every value a pass mutates.
struct State {
    asc: Option<AscBackend>,
    presenter: Option<Presenter>,
    free_inputs: VecDeque<usize>,
    pending_aus: VecDeque<Frame>,
    /// Phase-lock v3: per-AU arrival stamps for the circular arrival-lead report (drained 1 Hz).
    arrival_stamps: Vec<i128>,
    ready: Vec<OutputReady>,
    applied_ds: Option<DataSpace>,
    fed: u64,
    rendered: u64,
    discarded: u64,
    /// AUs larger than the codec input buffer, dropped whole (see `feed`).
    oversized_dropped: u64,
    /// Slice-progressive continuity ledger (see `PartFeed`).
    part_open: Option<PartFeed>,
    /// pts → realtime ns at the AU's LAST piece entering the codec — the P3 decode-split ledger
    /// (`feed` = received→queued, `codec` = queued→decoded). Always on; consumed by `present`.
    queued_stamps: VecDeque<(u64, i128)>,
    /// Freeze-until-reanchor gate. Armed on a frame-index gap (the feeder's Au verdict), a
    /// parked-AU overflow drop, a dropped-count climb, a recoverable codec error, or a rebuild;
    /// `recovery_flags` carries each AU's user_flags from `dispatch` (feed) to `present`, keyed
    /// by the codec-echoed pts.
    gate: ReanchorGate,
    last_arms: u64,
    recovery_flags: VecDeque<(u64, u32)>,
    fatal: bool,
    /// The codec stopped taking input with AUs waiting: this run ends and the codec is rebuilt.
    wedged: bool,
    stall: InputStall,
    /// A rebuilt run drops every AU before the first keyframe.
    await_keyframe: bool,
    backstops: Backstops,
}

impl State {
    fn new(asc: Option<AscBackend>, presenter: Option<Presenter>, gate: ReanchorGate) -> State {
        State {
            asc,
            presenter,
            free_inputs: VecDeque::new(),
            pending_aus: VecDeque::new(),
            arrival_stamps: Vec::new(),
            ready: Vec::new(),
            applied_ds: None,
            fed: 0,
            rendered: 0,
            discarded: 0,
            oversized_dropped: 0,
            part_open: None,
            queued_stamps: VecDeque::new(),
            last_arms: gate.arms(),
            gate,
            recovery_flags: VecDeque::new(),
            fatal: false,
            wedged: false,
            stall: InputStall::default(),
            await_keyframe: false,
            backstops: Backstops::new(),
        }
    }

    /// Route one [`DecodeEvent`] into the working sets.
    fn dispatch(&mut self, ev: DecodeEvent, pass: &mut Pass) {
        match ev {
            DecodeEvent::Au(f, gap) => {
                // A forward frame-index gap arms the freeze; park this AU's flags for the present
                // side to fold `on_decoded`. Credited arm: the gap width pre-covers the
                // reassembler's later `frames_dropped` climb for the same loss, so a fast RFI
                // anchor that heals in between isn't re-frozen by it.
                if gap > 0 {
                    self.gate
                        .arm_expecting_drops(Instant::now(), u64::from(gap));
                }
                if self.await_keyframe {
                    if f.flags & u32::from(FLAG_SOF) == 0 {
                        return;
                    }
                    self.await_keyframe = false;
                }
                // One entry per AU (parts share the pts): the completing delivery carries it.
                // Its arrival stamp is the phase the host's hold actually moves — prefix parts
                // would smear the phase toward the first slice's landing.
                if f.complete {
                    self.recovery_flags.push_back((f.pts_ns / 1000, f.flags));
                    if self.recovery_flags.len() > IN_FLIGHT_CAP {
                        self.recovery_flags.pop_front();
                    }
                    self.arrival_stamps.push(if f.received_ns > 0 {
                        f.received_ns as i128
                    } else {
                        now_realtime_ns()
                    });
                    if self.arrival_stamps.len() > 256 {
                        self.arrival_stamps.remove(0);
                    }
                }
                self.pending_aus.push_back(f);
                if self.pending_aus.len() > FRAME_PARK_CAP {
                    self.pending_aus.pop_front(); // sustained overflow — drop oldest
                    pass.aus_dropped += 1;
                }
            }
            DecodeEvent::InputAvailable(i) => {
                self.free_inputs.push_back(i);
                pass.input_offered = true;
            }
            DecodeEvent::OutputAvailable {
                index,
                pts_us,
                decoded_ns,
                decoded_mono_ns,
            } => self.ready.push(OutputReady {
                index,
                pts_us,
                decoded_ns,
                decoded_mono_ns,
            }),
            DecodeEvent::FormatChanged => pass.fmt_dirty = true,
            DecodeEvent::Vsync => pass.vsync_tick = true,
            DecodeEvent::Error { fatal: true } => self.fatal = true,
            // A recoverable/transient codec error is a decode hiccup on a broken reference chain —
            // arm the freeze so the concealed output it recovers into is held off the screen.
            DecodeEvent::Error { fatal: false } => self.gate.arm(Instant::now()),
            DecodeEvent::PresentComplete(pc) => pass.present_completes.push(pc),
        }
    }

    /// The pass proper, after the drain: completions, the vsync tick, the format change, feeding,
    /// the re-anchor cadence reset, then presenting.
    fn apply(&mut self, ctx: &Ctx, pass: &mut Pass) {
        if let Some(a) = self.asc.as_mut() {
            let off = ctx.offset();
            for pc in pass.present_completes.drain(..) {
                a.on_present_complete(pc, off, &ctx.stats, &ctx.video_e2e);
            }
        }
        if pass.vsync_tick {
            if let Some(p) = self.presenter.as_mut() {
                p.on_vsync();
            }
            if let Some(a) = self.asc.as_mut() {
                a.poll_fences(ctx.offset(), &ctx.stats, &ctx.video_e2e);
            }
        }
        ctx.stats.note_skipped_overflow(pass.aus_dropped); // parked-AU overflow: skips, flagged as such
        if pass.fmt_dirty {
            if let Some((w, h)) = super::display::picture_size(&ctx.codec) {
                ctx.decoded_size
                    .store(crate::session::pack_surface_size(w, h), Ordering::Relaxed);
            }
            match self.asc.as_mut() {
                // ASC carries the colour on the transaction, not the SurfaceView window. Refine
                // only when the codec reports a transfer — a `None` echo (decoders commonly omit
                // `color-transfer`) must not clobber the negotiated dataspace.
                Some(a) => {
                    if let Some(ds) = reported_dataspace(&ctx.codec) {
                        a.set_dataspace(i32::from(ds));
                    }
                }
                None => apply_reported_dataspace(&ctx.codec, &ctx.window, &mut self.applied_ds),
            }
        }
        self.feed(ctx);
        // The cadence loop's re-anchor seam. A fresh arm means a loss was detected: the frames
        // that reach the presenter on the far side come through a decoder that has just
        // recovered, so the source→presentable delay the loop had measured is not the one it
        // will see. Watched by ARM COUNT because the arm sites are spread across the dispatcher,
        // the feeder and the backstops, and the count catches every one of them.
        if self.gate.arms() != self.last_arms {
            self.last_arms = self.gate.arms();
            if let Some(p) = self.presenter.as_mut() {
                p.reset_cadence();
            }
            if let Some(a) = self.asc.as_mut() {
                a.reset_cadence();
            }
        }
        self.present(ctx);
    }

    /// Queue as many parked AUs as there are free input buffer slots (async mode: the indices
    /// come from `InputAvailable` callbacks, not a dequeue). Each AU is copied into its codec
    /// input buffer and submitted; an AU larger than the buffer is DROPPED (+ a recovery keyframe
    /// requested) — a truncated AU is corrupt input the decoder chews on silently, poisoning the
    /// reference chain.
    ///
    /// Slice-progressive deliveries ([`Frame::part`]) feed as they arrive: every piece rides
    /// [`BUFFER_FLAG_PARTIAL_FRAME`] except the AU's last, all at the AU's pts. `part_open` is
    /// the continuity ledger — any break (gap, orphan, oversize) abandons the AU per
    /// [`PartFeed::pts_us`]'s close contract and re-syncs at the next `first`.
    fn feed(&mut self, ctx: &Ctx) {
        let codec = &ctx.codec;
        while !self.pending_aus.is_empty() && !self.free_inputs.is_empty() {
            let idx = self.free_inputs.pop_front().unwrap();
            let frame = self.pending_aus.pop_front().unwrap();
            let pts_us = frame.pts_ns / 1000;
            let (first, last, offset) = match frame.part {
                None => (true, true, 0usize),
                Some(p) => (p.first, p.last, p.offset as usize),
            };
            // Continuity ledger. `continues` = this piece extends the open AU exactly;
            // anything else with an AU open means that AU died mid-flight and must be closed
            // (empty non-PARTIAL buffer at ITS pts) before this frame may touch the codec.
            let continues = self
                .part_open
                .as_ref()
                .is_some_and(|o| frame.frame_index == o.index && offset == o.expected && !first);
            if !continues {
                if let Some(o) = self.part_open.take() {
                    // Spend THIS slot on the close; the current frame re-queues for the next one.
                    if let Err(e) = codec.queue_input_buffer_by_index(idx, 0, 0, o.pts_us, 0) {
                        log::warn!("decode: close of abandoned partial AU {}: {e}", o.index);
                    }
                    log::warn!(
                        "decode: partial AU {} abandoned mid-feed — closed empty, requesting keyframe",
                        o.index
                    );
                    // The close makes the codec emit concealed garbage at the dead pts — freeze it
                    // off the glass until the recovery keyframe re-anchors.
                    self.gate.arm(Instant::now());
                    let _ = ctx.client.request_keyframe();
                    self.pending_aus.push_front(frame);
                    continue;
                }
                // No AU open: an orphan non-first piece lost its head upstream — discard and
                // re-sync at the next `first` (the recovery request rides the same loss).
                if !first {
                    self.free_inputs.push_front(idx);
                    self.gate.arm(Instant::now());
                    let _ = ctx.client.request_keyframe();
                    continue;
                }
            }
            let Some(dst) = codec.input_buffer(idx) else {
                // Nothing was written and nothing was queued, so BOTH stay ours: forgetting the
                // slot leaks one of the codec's input buffers per occurrence, and dropping the AU
                // punches a hole in the reference chain with no keyframe request behind it.
                // `break`, not `continue`: a codec that cannot hand out an input buffer it just
                // advertised is in no state to be fed the rest of the parked queue this pass.
                log::warn!("decode: input_buffer({idx}) returned None — retrying next pass");
                self.free_inputs.push_front(idx);
                self.pending_aus.push_front(frame);
                break;
            };
            let au = &frame.data;
            if au.len() > dst.len() {
                // The slot was never queued, so it stays ours — recycle it for the next AU.
                self.free_inputs.push_front(idx);
                self.oversized_dropped += 1;
                log::warn!(
                    "decode: AU {} > input buffer {} — dropped ({} so far), requesting keyframe",
                    au.len(),
                    dst.len(),
                    self.oversized_dropped
                );
                let _ = ctx.client.request_keyframe();
                if frame.part.is_some() {
                    self.gate.arm(Instant::now());
                    // Pieces already queued can't be unqueued: poison the ledger so the next
                    // delivery mismatches and takes the close-empty path above.
                    self.part_open = Some(PartFeed {
                        index: frame.frame_index,
                        expected: usize::MAX,
                        pts_us,
                    });
                }
                continue;
            }
            let n = au.len();
            // SAFETY: `au` (wire AU) and `dst` (codec input buffer) are distinct allocations, both
            // valid for `n` bytes; `MaybeUninit<u8>` is layout-identical to `u8`, so this
            // initializes dst[..n].
            unsafe {
                std::ptr::copy_nonoverlapping(au.as_ptr(), dst.as_mut_ptr().cast::<u8>(), n);
            }
            let flags = if last { 0 } else { BUFFER_FLAG_PARTIAL_FRAME };
            if let Err(e) = codec.queue_input_buffer_by_index(idx, 0, n, pts_us, flags) {
                log::warn!("decode: queue_input_buffer_by_index: {e}");
                if frame.part.is_some() && !last {
                    // The piece never reached the codec — same unrecoverable-AU shape as oversize.
                    self.part_open = Some(PartFeed {
                        index: frame.frame_index,
                        expected: usize::MAX,
                        pts_us,
                    });
                }
                continue;
            }
            // `fed` counts ACCESS UNITS toward the HUD's fed/decoded balance — the closing piece
            // (or a whole AU) bumps it. The queued stamp marks the same instant (the AU is fully
            // in the codec's hands): the P3 decode split measures `codec` from here, so a
            // slice-progressive head start shows up as codec-pure shrink.
            if last {
                self.fed += 1;
                self.queued_stamps.push_back((pts_us, now_realtime_ns()));
                if self.queued_stamps.len() > IN_FLIGHT_CAP {
                    self.queued_stamps.pop_front(); // stale — codec never echoed it back
                }
            }
            self.part_open = if last {
                None
            } else {
                Some(PartFeed {
                    index: frame.frame_index,
                    expected: offset + n,
                    pts_us,
                })
            };
        }
    }

    /// Pair each ready output's decode stage: the ABR decode signal + the HUD histogram consume
    /// the receipt map; the P3 split's codec-pure half needs only the queued stamp, so it records
    /// even with both off (that keeps the 1 Hz pf.present mirror HUD-off readable). `meter` is
    /// the SurfaceView path's always-on e2e sink; the ASC backend keeps its own 1 Hz line.
    fn note_decode_stage(&mut self, ctx: &Ctx, meter: Option<&PresentMeter>) {
        let want_stage = ctx.stats.enabled() || ctx.measure_decode;
        let clock_offset = ctx.offset();
        let mut g = ctx
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for o in &self.ready {
            let received_ns = if want_stage {
                note_decoded_pts(
                    &ctx.client,
                    ctx.measure_decode,
                    &ctx.stats,
                    &mut g,
                    o.pts_us,
                    o.decoded_ns,
                )
            } else {
                None
            };
            let queued = take_stamp(&mut self.queued_stamps, o.pts_us);
            let codec_us = queued.map(|q| ((o.decoded_ns - q).max(0) / 1000) as u64);
            let feed_us = match (queued, received_ns) {
                (Some(q), Some(r)) => Some(((q - r).max(0) / 1000) as u64),
                _ => None,
            };
            if let Some(m) = meter {
                // Same formula + clamp as the HUD's capture→decoded headline in `note_decoded_pts`.
                let e2e_ns = o.decoded_ns + clock_offset as i128 - o.pts_us as i128 * 1000;
                let e2e_us =
                    (e2e_ns > 0 && e2e_ns < 10_000_000_000).then_some((e2e_ns / 1000) as u64);
                m.note_decode(feed_us, codec_us, e2e_us);
            }
        }
    }

    /// Route the ready outputs toward glass, recording each one's decode-split + e2e first, then
    /// folding EVERY output through the re-anchor gate in pts (== decode) order — even the ones
    /// newest-wins discards — so the two-mark re-anchor count stays correct. A withheld verdict is
    /// concealment: released unrendered (the SurfaceView keeps the last frame frozen on) or
    /// dropped off-glass (ASC). With the timeline presenter the approved ones go to its store and
    /// `Presenter::pump` releases them budgeted; legacy `arrival` presents only the NEWEST
    /// immediately. `ready` is drained.
    fn present(&mut self, ctx: &Ctx) {
        if self.ready.is_empty() {
            return;
        }
        // The ASC backend keeps its own 1 Hz line, so only the SurfaceView path feeds the meter.
        let meter = self.asc.is_none().then_some(&*ctx.meter);
        self.note_decode_stage(ctx, meter);
        let codec = &ctx.codec;
        let now = Instant::now();
        let mut skipped: u64 = 0;
        let ready = std::mem::take(&mut self.ready);
        if let Some(a) = self.asc.as_mut() {
            // The pump composites the rendered images onto the layer; the display stage is
            // measured there from the real transaction latches, not here.
            for o in ready {
                let flags = take_flags(&mut self.recovery_flags, o.pts_us);
                let present = self.gate.on_decoded(flags, false, now) == GateVerdict::Present;
                skipped += u64::from(!present);
                a.on_output(
                    codec,
                    o.index,
                    o.pts_us,
                    o.decoded_ns,
                    o.decoded_mono_ns,
                    present,
                );
            }
            // Gate-withheld frames only (the reader-drop skips ride `asc.flush`).
            ctx.stats.note_skipped(skipped);
            return;
        }
        if let Some(p) = self.presenter.as_mut() {
            for o in ready {
                let flags = take_flags(&mut self.recovery_flags, o.pts_us);
                if self.gate.on_decoded(flags, false, now) == GateVerdict::Present {
                    let dropped =
                        p.submit(codec, o.index, o.pts_us, o.decoded_ns, o.decoded_mono_ns);
                    skipped += dropped;
                    self.discarded += dropped;
                } else {
                    if let Err(e) = codec.release_output_buffer_by_index(o.index, false) {
                        log::warn!("decode: release_output_buffer_by_index({}): {e}", o.index);
                    }
                    self.discarded += 1;
                    skipped += 1;
                }
            }
        } else {
            let last = ready.len() - 1;
            for (i, o) in ready.into_iter().enumerate() {
                let flags = take_flags(&mut self.recovery_flags, o.pts_us);
                let present = self.gate.on_decoded(flags, false, now) == GateVerdict::Present;
                let render = i == last && present;
                match codec.release_output_buffer_by_index(o.index, render) {
                    Ok(()) if render => {
                        self.rendered += 1;
                        ctx.tracker
                            .note_rendered(o.pts_us, o.decoded_ns, now_realtime_ns());
                    }
                    Ok(()) => {
                        self.discarded += 1;
                        skipped += 1;
                    }
                    Err(e) => log::warn!(
                        "decode: release_output_buffer_by_index({}, {render}): {e}",
                        o.index
                    ),
                }
            }
        }
        ctx.stats.note_skipped(skipped); // HUD `skipped` counter (newest-wins + held-off drops)
    }

    /// The backends' decision point, run EVERY pass — frame arrivals, vsync ticks and the 5 ms
    /// housekeeping wake all land here, which is what reopens the glass budget on time even when
    /// the choreographer clock is absent. The ASC backend phases on its own present fences and
    /// takes only the panel period from the choreographer.
    fn pump(&mut self, ctx: &Ctx, clock: Option<&VsyncShared>) {
        if let Some(p) = self.presenter.as_mut() {
            let now = now_monotonic_ns();
            if p.pump(&ctx.codec, clock, &ctx.tracker, &ctx.meter, now) {
                self.rendered += 1;
            }
            // The 1 Hz window flush doubles as the phase-lock report tick.
            if let (Some(_), Some(c)) = (p.flush_log(&ctx.meter, clock), clock) {
                report_arrival_phase(ctx, c, &mut self.arrival_stamps);
            }
        }
        if let Some(a) = self.asc.as_mut() {
            if let Some(tx) = ctx.present_tx.as_ref() {
                let panel = clock.map_or(0, VsyncShared::panel_period_ns);
                if a.pump(now_monotonic_ns(), panel, tx) {
                    self.rendered += 1;
                }
            }
            a.flush(&ctx.stats);
        }
    }

    /// The first-frame line separates "the stream never reached glass" from "it reached glass and
    /// looked wrong"; the periodic tally only starts at 300 frames.
    fn log_progress(&self) {
        let State {
            fed,
            rendered,
            discarded,
            ..
        } = self;
        if *rendered == 1 {
            log::info!("decode: first frame presented (fed={fed} discarded={discarded})");
        }
        if *rendered > 0 && *rendered % 300 == 0 {
            log::info!("decode: fed={fed} rendered={rendered} discarded={discarded}");
        }
    }

    /// The hung-codec check ([`InputStall`]), then the keyframe backstops ([`Backstops::poll`]).
    /// Evaluated after `feed`, so an AU that arrived this pass has either been fed or is parked
    /// in `pending_aus`.
    fn housekeeping(&mut self, ctx: &Ctx, had_output: bool, pass: &Pass) {
        let waiting = !self.pending_aus.is_empty();
        if self.stall.poll(pass.input_offered, waiting, Instant::now()) {
            log::warn!(
                "decode: the codec took no input for {} ms with {} AU(s) waiting — treating it as hung",
                INPUT_STALL_PATIENCE.as_millis(),
                self.pending_aus.len()
            );
            self.wedged = true;
        }
        self.backstops.poll(
            &ctx.client,
            &mut self.gate,
            self.fed,
            had_output,
            waiting,
            pass.aus_dropped,
        );
    }
}

/// Phase-lock v3 sensor: the CIRCULAR mean + coherence of the ARRIVAL lead — each AU's reassembly
/// stamp against the panel's latch grid — because arrival is the phase the host actually
/// controls (the v2 latch statistic measured downstream of the decoder pipeline, which absorbed
/// the actuation). Timestamps convert monotonic→realtime→host; the skew offset lives client-side.
/// `stamps` is drained.
fn report_arrival_phase(ctx: &Ctx, clock: &VsyncShared, stamps: &mut Vec<i128>) {
    let period = clock.panel_period_ns().max(clock.period_ns());
    if period <= 0 {
        return;
    }
    let Some(t) = clock.next_target(now_monotonic_ns()) else {
        return;
    };
    let mono_now = now_monotonic_ns();
    let real_now = now_realtime_ns();
    let leads_us: Vec<u64> = stamps
        .drain(..)
        .map(|r_ns| {
            let arrival_mono = mono_now as i128 - (real_now - r_ns);
            ((t.expected_present_ns as i128 - arrival_mono).rem_euclid(period as i128) / 1000)
                as u64
        })
        .collect();
    let Some((lead_mean_ns, coherence)) = punktfunk_core::phase::circular_latch(&leads_us, period)
    else {
        return;
    };
    log::info!(
        target: "pf.phase",
        "arrival lead circ={:.2}ms coh={}",
        lead_mean_ns as f64 / 1e6,
        coherence
    );
    let latch_real_ns = real_now + (t.expected_present_ns - mono_now) as i128;
    let latch_host_ns = (latch_real_ns + ctx.offset() as i128).max(0) as u64;
    ctx.client.report_phase(
        latch_host_ns,
        period.clamp(0, u32::MAX as i64) as u32,
        1_000_000, // skew residual — conservative 1 ms
        lead_mean_ns.min(u32::MAX as u64) as u32,
        coherence,
    );
}

/// The `pf-decode-feed` thread: block on the connector for the next access unit so the async loop
/// never has to. Stamps receipt for the decode stage (the connector already noted it for the
/// overlay), then hands the AU to the loop via the event channel. Exits when `shutdown` is set,
/// the session closes, or the loop's receiver is gone.
fn feeder_loop(
    client: Arc<NativeClient>,
    stats: Arc<crate::stats::VideoStats>,
    measure_decode: bool,
    in_flight: Arc<Mutex<VecDeque<(u64, i128)>>>,
    shutdown: Arc<AtomicBool>,
    ev_tx: mpsc::Sender<DecodeEvent>,
) {
    // Last logged phase-lock ACK (the host's applied capture hold, from the 0xCF tail).
    let mut last_phase_ack: Option<i32> = None;
    while !shutdown.load(Ordering::Relaxed) {
        match client.next_frame(Duration::from_millis(5)) {
            Ok(frame) => {
                // Loss recovery (RFI): a forward frame-index gap fires a throttled reference-frame-
                // invalidation request (a cheap clean P-frame instead of a full IDR); the verdict
                // rides the Au event so the loop arms its freeze gate on the same signal. Parts
                // repeat their AU's index — note it once, on the first piece.
                let au_first = frame.part.is_none_or(|p| p.first);
                let gap = if au_first {
                    client.note_frame_index(frame.frame_index)
                } else {
                    0
                };
                // Park the receipt stamp whenever the `decode` stage is consumed: the HUD, or the
                // ABR decode signal (`measure_decode`).
                if (stats.enabled() || measure_decode) && frame.complete {
                    let received_ns = note_received_frame(&client, &frame, &mut last_phase_ack);
                    let mut g = in_flight
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    g.push_back((frame.pts_ns / 1000, received_ns));
                    if g.len() > IN_FLIGHT_CAP {
                        g.pop_front(); // stale — codec never echoed it back
                    }
                }
                if ev_tx.send(DecodeEvent::Au(frame, gap)).is_err() {
                    break; // the decode loop is gone
                }
            }
            Err(PunktfunkError::NoFrame) => {} // timeout — re-check shutdown and poll again
            Err(_) => break,                   // session closed
        }
    }
}

/// `AMEDIACODEC_BUFFER_FLAG_PARTIAL_FRAME` (NDK ≥ 26, gated by the Kotlin
/// `FEATURE_PartialFrame` probe): this input buffer is a PIECE of an AU — the codec assembles
/// pieces until a buffer WITHOUT the flag closes the AU.
const BUFFER_FLAG_PARTIAL_FRAME: u32 = 8;

/// The slice-progressive feed's open access unit: parts already queued into the codec under
/// [`BUFFER_FLAG_PARTIAL_FRAME`], awaiting the rest. Loop-local — a codec rebuild tears the
/// whole loop down, so the state can never outlive the codec instance it fed.
pub(super) struct PartFeed {
    index: u32,
    /// The AU byte offset the next part must carry — a mismatch means the hand-off dropped a
    /// piece (memory cap / jump-to-live clear) and the AU is unrecoverable.
    expected: usize,
    /// The dead-close pts: an abandoned AU is CLOSED with an empty non-PARTIAL buffer at its
    /// own pts — the codec then emits (concealed garbage) at that pts, which the reanchor
    /// freeze gate withholds from glass while the keyframe request recovers the chain. No
    /// mid-stream codec flush needed.
    pts_us: u64,
}

#[cfg(test)]
mod tests {
    use super::bring_up_rungs;

    /// The ladder that turns a decoder which refuses to configure or start into a retry or two
    /// away from a picture. Order and de-duplication are the whole of its logic — everything else
    /// in the loop is MediaCodec I/O.
    #[test]
    fn rungs_shed_the_overlay_then_asc_then_the_keys_and_never_repeat_one() {
        // The default: shed the reader's COMPOSER_OVERLAY usage first (keeping ASC — the whole
        // point of the middle rung), then the `AImageReader` entirely, then the aggressive keys,
        // then every low-latency key.
        assert_eq!(
            bring_up_rungs(true, true),
            [
                (Some(true), Some(true)),
                (Some(false), Some(true)),
                (None, Some(true)),
                (None, Some(false)),
                (None, None)
            ]
        );
        // `present_backend=surfaceview` already shed ASC — both ASC rungs collapse away.
        assert_eq!(
            bring_up_rungs(false, true),
            [(None, Some(true)), (None, Some(false)), (None, None)]
        );
        // Low-latency mode off ⇒ the keys are already the plain set; the aggressive rung collapses.
        assert_eq!(
            bring_up_rungs(true, false),
            [
                (Some(true), Some(false)),
                (Some(false), Some(false)),
                (None, Some(false)),
                (None, None)
            ]
        );
        // Only the keys left to shed: one retry, and no second `start` of the same thing.
        assert_eq!(
            bring_up_rungs(false, false),
            [(None, Some(false)), (None, None)]
        );

        for asc in [true, false] {
            for ll in [true, false] {
                let rungs = bring_up_rungs(asc, ll);
                // A device that works must pay nothing for this ladder: rung 0 is always exactly
                // what the session asked for.
                assert_eq!(rungs[0], (asc.then_some(true), Some(ll)));
                // Every ladder ends at the most conservative configuration there is.
                assert_eq!(*rungs.last().unwrap(), (None, None));
                // Monotonic: a rung only ever sheds, never re-enables what an earlier one dropped
                // (`Option<bool>`'s Ord: `None < Some(false) < Some(true)`), so the ladder always
                // descends towards the conservative end.
                assert!(rungs
                    .windows(2)
                    .all(|w| w[1].0 <= w[0].0 && w[1].1 <= w[0].1));
            }
        }
    }
}
