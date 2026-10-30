//! The event-driven async MediaCodec decode loop (default) + its feeder/dispatch/present helpers.

use ndk::media::media_codec::{AsyncNotifyCallback, MediaCodec, MediaCodecDirection};
use ndk::native_window::NativeWindow;
use punktfunk_core::client::{Decoded, NativeClient};
use punktfunk_core::error::PunktfunkError;
use punktfunk_core::reanchor::{AnchorEvidence, DecoderClass, LocalRecovery};
use punktfunk_core::session::Frame;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, io};

use super::display::{
    install_render_callback, layer_dataspace, release_render_callback, DisplayTracker,
};
use super::latency::{
    note_decoded_pts, note_received_frame, now_realtime_ns, take_by_pts, take_flags,
};
use super::setup::{
    boost_hot_threads, boost_thread_priority, codec_mime, create_codec, hdr_static,
    low_latency_format, try_set_frame_rate,
};
use super::sink::{self, asc_backend_selected, AscSink, PresentMeter, Sink};
use super::surface_control::{Layer, PresentComplete};
use super::vsync::{VsyncClock, VsyncShared};
use super::{Backstops, DecodeOptions, FRAME_PARK_CAP, IN_FLIGHT_CAP};
use crate::au_intake::{hand_on, Delivery, Intake};
use crate::feed_rules::{
    colour_after, gate, render_after, spoiled, Cause, Gate, PartFeed, Piece, Spoiled,
};
use crate::input_stall::{
    rung_after_failure, InputStall, OutputSilence, RungFloor, INPUT_STALL_PATIENCE, SETTLED_FRAMES,
};
use crate::present::verdict::Verdict;
use crate::run_end::{after_failure, ending, Ending, Retry};
use crate::rung_memory::{self, Entry, Key, Outcome, Start};
use crate::sys::{now_monotonic_ns, sysprop};
use punktfunk_core::quic::ColorInfo;

/// One decoded output buffer ready to release: its codec buffer index + the pts the codec echoed
/// (from the output callback's `BufferInfo`), used to pair the `decode` HUD stat, and the
/// wall-clock instant the output callback fired — the spec's `decoded` point ("decoder output
/// frame available"), stamped at the callback so the event-channel hop + coalescing wait in the
/// loop never inflates the decode stage.
pub(super) struct OutputReady {
    pub(super) index: usize,
    pub(super) pts_us: u64,
    pub(super) decoded_ns: i128,
    pub(super) decoded_mono_ns: i64,
}

/// Events the async decode loop reacts to. The codec's async-notify callbacks (which run on its
/// internal looper thread) push the codec ones; the feeder thread pushes `Au`. Each carries only
/// owned/`Copy` data so the callback closures satisfy the `Send` bound and never touch the codec.
pub(crate) enum DecodeEvent {
    /// A received access unit core admitted, ready to queue into the decoder. The `u32` is
    /// the gap ahead of it from [`NativeClient::recovery_on_au`] (0 = none): the receiver rule
    /// and the ladder's loss mark key on it. Core armed the freeze and asked at the report.
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
    /// A rendered frame reached the ASC reader. Only wakes the loop: its pass drains the reader.
    ImageAvailable,
    /// An `ASurfaceControl` transaction completed (ASurfaceControl backend only): the real latch
    /// time + the previous buffer's release fence, forwarded from the completion callback (a binder
    /// thread) so the decode loop applies it on its own thread.
    PresentComplete(super::surface_control::PresentComplete),
    /// The codec reported an error; `fatal` when neither recoverable nor transient.
    Error { fatal: bool },
}

/// The decoder bring-up rungs, in order, as `(present backend, low-latency keys)`.
/// The backend is `Some(overlay)` for ASC with that reader-usage profile (see
/// [`AscSink::create`]'s `overlay` doc), `None` for the SurfaceView presenter. The keys are
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

/// Human label for a rung's low-latency keys, for the same log lines.
fn keys_label(keys: Option<bool>) -> &'static str {
    match keys {
        Some(true) => "aggressive low-latency keys ON",
        Some(false) => "aggressive low-latency keys OFF",
        None => "no low-latency keys",
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

/// How one [`run_codec`] ended.
enum RunEnd {
    /// The session stopped video, or every bring-up rung refused the first codec.
    Stopped,
    /// The codec died or stopped taking input after showing `rendered` frames. `clean`: it was
    /// fed nothing past a loss and reported no error. `lower`: the ladder rung its replacement
    /// starts at ([`rung_after_failure`]), if there is one.
    Failed {
        rendered: u64,
        clean: bool,
        lower: Option<usize>,
    },
    /// The host changed the stream's format at this frame: the next codec is built for it and
    /// takes the frame first.
    Reformat(Box<Frame>),
}

/// The rung memory's file in the app's files dir ([`crate::rung_memory`]).
const MEMORY_FILE: &str = "decode-rungs.txt";

/// This surface's copy of the rung memory: the file's entries, the stream codec's key, and
/// where that codec's decoders started. `path` is `None` with no files dir or once a write fails.
struct Memory {
    path: Option<PathBuf>,
    entries: Vec<Entry>,
    key: Key,
    start: Start,
}

impl Memory {
    /// Read the file. One that is missing or unreadable is no memory.
    fn load() -> Memory {
        let path = crate::logs::dir().map(|dir| dir.join(MEMORY_FILE));
        let entries = match path.as_deref().map(fs::read_to_string) {
            Some(Ok(text)) => rung_memory::parse(&text),
            Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => {
                log::debug!("decode: rung memory unreadable: {e}");
                Vec::new()
            }
            _ => Vec::new(),
        };
        Memory {
            path,
            entries,
            key: Key::default(),
            start: Start::Fresh,
        }
    }

    /// Key the memory on the stream's codec: the floor its first decoder starts on.
    fn begin(&mut self, client: &NativeClient, opts: &DecodeOptions) -> RungFloor {
        let mime = codec_mime(client.video_format().codec);
        self.key = Key {
            fingerprint: sysprop(c"ro.build.fingerprint")
                .or_else(|| sysprop(c"ro.build.version.incremental"))
                .unwrap_or_default(),
            // Kotlin's pick, known before bring-up and stable per device; a probe codec costs
            // 100 ms+ on a TV.
            decoder: super::ranked_for(opts.decoder_name.as_deref(), opts.decoder_mime, mime)
                .unwrap_or("default")
                .to_owned(),
            codec: mime.to_owned(),
        };
        let now = unix_secs();
        self.start = rung_memory::lookup(&self.entries, &self.key, now, APP_VERSION);
        let Key { decoder, codec, .. } = &self.key;
        if let Some(e) = rung_memory::find(&self.entries, &self.key) {
            let days = now.saturating_sub(e.proven_at_secs) / 86_400;
            match self.start {
                Start::Remembered(r) => log::info!(
                    "decode: starting at rung {r} from memory ({decoder} / {codec}, proven {days} \
                     days ago)"
                ),
                Start::Retry(_) => log::info!(
                    "decode: retrying rung 0 for {decoder} / {codec} (memory from {}, {days} \
                     days old)",
                    e.version
                ),
                Start::Fresh => {}
            }
        }
        RungFloor::starting_at(self.start.rung())
    }

    /// A decoder has shown `rendered` frames with the floor at `floor`: write what it proved.
    fn shown(&mut self, floor: usize, rendered: u64) {
        let Key { decoder, codec, .. } = &self.key;
        let changed = match rung_memory::outcome(self.start, floor, rendered) {
            Outcome::Keep => return,
            Outcome::Record(r) => {
                log::info!("decode: rung {r} remembered for {decoder} / {codec}");
                self.start = Start::Remembered(r);
                rung_memory::record(&mut self.entries, &self.key, r, unix_secs(), APP_VERSION)
            }
            Outcome::Forget => {
                log::info!("decode: rung 0 works again for {decoder} / {codec} — memory cleared");
                self.start = Start::Fresh;
                rung_memory::forget(&mut self.entries, &self.key)
            }
        };
        if let Some(path) = self.path.as_deref().filter(|_| changed) {
            if let Err(e) = fs::write(path, rung_memory::render(&self.entries)) {
                log::debug!("decode: rung memory unwritable: {e}");
                self.path = None;
            }
        }
    }
}

/// The version a remembered rung was proven under.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The decode thread's body (see [`run`]): one [`run_codec`] per codec. A codec that dies or
/// hangs mid-session is torn down and rebuilt on the same surface, so the session keeps its
/// video instead of freezing until the user reconnects. Codecs that keep failing on an unbroken
/// stream, on a rung where none ever settled, move the rebuild down the bring-up ladder
/// ([`RungFloor`]). The floor starts where the rung memory ([`Memory`]) puts it, and a floor
/// that moved and then settled is written back for the next video start. Past
/// [`MAX_BARREN_REBUILDS`](crate::run_end::MAX_BARREN_REBUILDS) failed runs that showed nothing,
/// the loop ends ([`after_failure`]).
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
    let mut memory = Memory::load();
    let mut floor = memory.begin(&client, &opts);
    // The failed codec's layer, still showing the last good frame until a rebuilt one presents.
    let mut stale: Option<Layer> = None;
    // Monotonic ns of the feeder's last loss gap (each one fires an RFI ask); 0 = none yet.
    let last_gap_ns = Arc::new(AtomicI64::new(0));
    let (mut rebuilds, mut after_gap) = (0u32, 0u32);
    let mut carry: Option<Box<Frame>> = None;
    loop {
        match run_codec(
            &client,
            &window,
            &shutdown,
            &stats,
            &opts,
            rebuilt && carry.is_none(),
            &mut stale,
            &last_gap_ns,
            floor.rung(),
            &mut memory,
            carry.take(),
        ) {
            RunEnd::Stopped => return,
            RunEnd::Reformat(frame) => {
                log::info!(
                    "decode: the host changed the stream format — rebuilding the decoder for {}",
                    codec_mime(client.video_format().codec)
                );
                // Another codec's failures say nothing about this one's rungs.
                (barren, floor) = (0, memory.begin(&client, &opts));
                carry = Some(frame);
            }
            RunEnd::Failed {
                rendered,
                clean,
                lower,
            } => {
                match after_failure(&mut barren, &mut floor, rendered, clean, lower) {
                    Retry::GiveUp => {
                        log::error!(
                            "decode: {barren} rebuilt decoders in a row showed no steady picture — \
                             giving up; video stays frozen until the stream restarts"
                        );
                        return;
                    }
                    Retry::Rebuild { stepped_down: true } => log::warn!(
                        "decode: two decoders in a row failed on an unbroken stream within \
                         {SETTLED_FRAMES} frames — the next one starts at rung {}",
                        floor.rung()
                    ),
                    Retry::Rebuild {
                        stepped_down: false,
                    } => {}
                }
                // Moonlight saw MediaTek HEVC decoders hang right after an RFI (GE8xxx GPUs).
                // The 5 s window is a reading aid for the report, not a verdict.
                rebuilds += 1;
                let gap_age_ms = match last_gap_ns.load(Ordering::Relaxed) {
                    0 => None,
                    t => Some(now_monotonic_ns().saturating_sub(t) / 1_000_000),
                };
                if gap_age_ms.is_some_and(|ms| ms < 5_000) {
                    after_gap += 1;
                }
                log::warn!(
                    "decode: rebuilding the decoder in place (rebuild {rebuilds}, {after_gap} \
                     within 5 s of a loss gap; last gap {}; it showed {rendered} frame(s) on {} \
                     stream)",
                    gap_age_ms.map_or("none".to_string(), |ms| format!("{ms} ms ago")),
                    if clean { "an unbroken" } else { "a broken" }
                );
                rebuilt = true;
            }
        }
    }
}

/// One codec's life: bring-up, the event loop, [`teardown`]. The codec drives the loop: an
/// async-notify callback fires the instant an input buffer frees or a frame finishes decoding,
/// so a decoded frame is presented without waiting out a poll interval. The callbacks run on the
/// codec's looper thread and only push events; every `AMediaCodec` buffer op stays on this
/// thread, which owns the codec. The [`Feeder`] thread blocks on the network so this loop never
/// does. How it ends is [`ending`]'s call, or [`no_codec`]'s when bring-up gave none.
///
/// Each run owns its event channel, so a dead codec's late callbacks cannot hand this run a
/// stale buffer index. `rebuilt`: a previous codec failed, so this one waits for a keyframe.
/// `stale`: a failed run's layer, hidden at this run's first present; a failed run that
/// presented leaves its own layer there. `last_gap_ns`: where the feeder stamps each loss gap,
/// for the rebuild log. `floor`: the ladder rung bring-up starts at. `memory`: told of every
/// presented frame, so a settled floor is written mid-run. `carry`: the first frame of a new
/// format, which ended the last run and is fed first.
#[allow(clippy::too_many_arguments)]
fn run_codec(
    client: &Arc<NativeClient>,
    window: &NativeWindow,
    shutdown: &AtomicBool,
    stats: &Arc<crate::stats::VideoStats>,
    opts: &DecodeOptions,
    rebuilt: bool,
    stale: &mut Option<Layer>,
    last_gap_ns: &Arc<AtomicI64>,
    floor: usize,
    memory: &mut Memory,
    carry: Option<Box<Frame>>,
) -> RunEnd {
    let mode = client.mode();
    // The event channel: the callbacks + feeder push, this loop pulls. `Sender` is `Send`, so the
    // callback closures (each capturing a clone) satisfy the async-notify `Send` bound.
    let (ev_tx, ev_rx) = mpsc::channel::<DecodeEvent>();
    let Some(Started {
        codec,
        asc,
        lower_rung,
        rung,
        backend,
    }) = bring_up(client, window, opts, &ev_tx, stats, floor)
    else {
        return no_codec(rebuilt, shutdown);
    };
    // ASC votes the rate on its own layer instead (the SurfaceView window shows nothing there).
    if asc.is_none()
        && mode.refresh_hz > 0
        && !try_set_frame_rate(window, mode.refresh_hz as f32, opts.force_rate_switch())
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
    // SurfaceFlinger's render timestamp. `render_cb` is the callback's leaked Arc refcount, which
    // [`teardown`] reclaims once the codec is gone. SurfaceView backend only — the ASC path
    // measures its display stage directly off the transaction completions.
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
    let sink = sink::select(
        asc,
        opts,
        mode.refresh_hz,
        window,
        &client.video_format().color,
    );
    // The SurfaceView timeline needs the vsync clock for its timelines, the ASC sink for the panel
    // period (its phase comes from present fences) and the fence poll on every tick.
    let vsync_tx = sink.needs_clock().then(|| ev_tx.clone());
    let mut aux = LoopAux::new(vsync_tx, opts, mode.refresh_hz);
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
        present_tx: sink.is_asc().then(|| ev_tx.clone()),
        decoded_size: opts.decoded_size.clone(),
        lower_rung,
        rung,
        backend,
    };
    // A fresh decoder holds no reference picture, so every P-frame before a keyframe is a
    // reference error, and reference errors can hang a hardware decoder. None reach this one.
    // A session's first decoder needs no ask: the stream opens on an IDR.
    let await_keyframe = rebuilt || opts.restart;
    let class = admission(
        client.video_format().codec,
        &ctx.codec.name().unwrap_or_default(),
    );
    let mut state = State::new(sink, Intake::new(class, await_keyframe));
    if await_keyframe {
        client.recovery_lost();
    }
    if let Some(mut frame) = carry {
        frame.new_format = None; // this codec is the one built for it
        state.dispatch(client, DecodeEvent::Au(*frame, 0), &mut Pass::default());
    }

    // `ev_tx` moves into the feeder: from here only it and the codec callbacks keep the channel up.
    let feeder = Feeder::spawn(
        client,
        stats,
        measure_decode,
        in_flight,
        ev_tx,
        last_gap_ns.clone(),
    );
    // ADPF: register this thread now. `aux` creates the session on the first presented frame, by
    // when the pump, audio and feeder threads have registered their tids too.
    client.register_hot_thread();

    while !shutdown.load(Ordering::Relaxed)
        && !state.fatal
        && !state.wedged
        && state.reformat.is_none()
    {
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
            state.dispatch(client, ev, &mut pass);
        }
        let had_output = !state.ready.is_empty();
        let rendered_before = state.rendered;
        state.apply(&ctx, &mut pass);
        state.pump(&ctx, aux.clock(), pass.lost);
        let presented_now = state.rendered > rendered_before;
        aux.after_pass(client, work_t0, had_output, presented_now);
        if presented_now {
            state.log_progress();
            memory.shown(floor, state.rendered);
            if let Some(mut layer) = stale.take() {
                layer.hide();
            }
        }
        state.housekeeping(&ctx, had_output, &pass);
    }
    let end = teardown(state, ctx, aux, render_cb, feeder, shutdown, stale);
    // The codec and its feeder are gone, so nothing reports: with the freeze left up, core
    // would ask for a keyframe every half second of a rebuild or a keep-alive drain.
    client.recovery_reset();
    end
}

/// The end of a run [`bring_up`] gave no codec. A first codec that every rung refused leaves
/// the session without video. A rebuild that finds none counts as a failed run, after the hung
/// codec's release gets half a second to free the hardware.
fn no_codec(rebuilt: bool, shutdown: &AtomicBool) -> RunEnd {
    let mut stopped = false;
    if rebuilt {
        for _ in 0..50 {
            stopped = shutdown.load(Ordering::Relaxed);
            if stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    match ending(None::<Frame>, rebuilt, stopped) {
        Ending::Failed => RunEnd::Failed {
            rendered: 0,
            clean: false,
            lower: None,
        },
        _ => RunEnd::Stopped,
    }
}

/// What the pass tail keeps between passes: the vsync clock, started lazily (see `vsync.rs`) with
/// its ticks on the event channel, and the ADPF hint session with the work it has yet to report.
struct LoopAux {
    vsync: Option<VsyncClock>,
    /// The clock's tick sender, taken when it starts; `None` from the outset without a presenter.
    vsync_tx: Option<mpsc::Sender<DecodeEvent>>,
    panel_hz: i32,
    hint: Option<crate::adpf::HintSession>,
    hint_tried: bool,
    refresh_hz: u32,
    low_latency_mode: bool,
    /// Productive (dispatch+feed+present) time since the last displayed frame. The blocking
    /// event wait is idle, not work.
    work_accum_ns: i64,
}

impl LoopAux {
    fn new(
        vsync_tx: Option<mpsc::Sender<DecodeEvent>>,
        opts: &DecodeOptions,
        refresh_hz: u32,
    ) -> LoopAux {
        LoopAux {
            vsync: None,
            vsync_tx,
            panel_hz: opts.panel_hz,
            hint: None,
            hint_tried: false,
            refresh_hz,
            low_latency_mode: opts.low_latency_mode,
            work_accum_ns: 0,
        }
    }

    fn clock(&self) -> Option<&VsyncShared> {
        self.vsync.as_ref().map(|v| v.shared().as_ref())
    }

    /// The pass's tail. The vsync clock starts on the first decoded output: started eagerly, it
    /// ticks the panel rate into a session that has no frame yet. A `None` from start (no
    /// choreographer surface) leaves ASAP targets. A presented frame reports the work since the
    /// last one to ADPF, which creates its session on the first.
    fn after_pass(
        &mut self,
        client: &NativeClient,
        pass_start: Instant,
        had_output: bool,
        presented: bool,
    ) {
        if had_output && self.vsync.is_none() {
            if let Some(tx) = self.vsync_tx.take() {
                self.vsync = VsyncClock::start(
                    self.panel_hz,
                    Box::new(move || {
                        let _ = tx.send(DecodeEvent::Vsync);
                    }),
                );
                if self.vsync.is_none() {
                    log::info!("decode: no choreographer clock — presenter uses ASAP targets");
                }
            }
        }
        self.work_accum_ns += pass_start.elapsed().as_nanos() as i64;
        if presented {
            if !self.hint_tried {
                self.hint_tried = true;
                self.hint = start_hint(client, self.refresh_hz, self.low_latency_mode);
            }
            if let Some(h) = &self.hint {
                h.report_actual(self.work_accum_ns);
            }
            self.work_accum_ns = 0;
        }
    }
}

/// End one run. The order is what keeps it sound: held output buffers and reader images go back
/// before the codec stops, the codec is deleted before the render callback's `Arc` is reclaimed,
/// and the feeder stops last. `render_cb` is the registration on `ctx.codec`. A run that ends
/// in a rebuild leaves its layer in `stale` when it presented ([`ending`]).
fn teardown(
    state: State,
    ctx: Ctx,
    aux: LoopAux,
    render_cb: Option<*const DisplayTracker>,
    feeder: Feeder,
    shutdown: &AtomicBool,
    stale: &mut Option<Layer>,
) -> RunEnd {
    let State {
        mut sink,
        fed,
        rendered,
        discarded,
        fatal,
        wedged,
        unclean,
        reformat,
        ..
    } = state;
    // Every held output buffer and reader image goes back before the codec stops.
    sink.release_all(&ctx.codec);
    drop(aux.vsync); // stop + join the choreographer thread; its channel sends are harmless after
    let lower = ctx.lower_rung;
    let _ = ctx.codec.stop();
    // AMediaCodec_delete — after this no render callback can fire. The ASC reader + layer outlive
    // the codec, which rendered into the reader's window. `into_stale_layer` then drops the reader;
    // the layer lives on only while a rebuild needs its last frame, and its control is freed once
    // every in-flight completion callback has also dropped its share.
    drop(ctx);
    let layer = sink.into_stale_layer();
    if let Some(ud) = render_cb {
        // SAFETY: `ud` is `install_render_callback`'s pointer for `ctx.codec`, which was dropped
        // above; this is its single reclaim.
        unsafe { release_render_callback(ud) };
    }
    // The feeder stops last: while a hung codec is slow to stop, it keeps the receive queue drained.
    feeder.stop_and_join();
    log::info!("decode: stopped (async, fed={fed} rendered={rendered} discarded={discarded})");
    let end = ending(reformat, fatal || wedged, shutdown.load(Ordering::Relaxed));
    // The last picture stays up until the replacement presents.
    if rendered > 0 && !matches!(end, Ending::Stopped) {
        *stale = layer;
    }
    match end {
        Ending::Reformat(frame) => RunEnd::Reformat(Box::new(frame)),
        Ending::Failed => RunEnd::Failed {
            rendered,
            clean: !unclean,
            lower,
        },
        Ending::Stopped => RunEnd::Stopped,
    }
}

/// A codec [`bring_up`] started, and where on the ladder it runs.
struct Started {
    codec: MediaCodec,
    asc: Option<AscSink>,
    /// The rung its replacement starts at should it fail ([`rung_after_failure`]).
    lower_rung: Option<usize>,
    rung: usize,
    /// The [`backend_label`] of the present backend that runs.
    backend: &'static str,
}

/// Climb the decoder bring-up ladder ([`bring_up_rungs`]) from `floor` until a codec starts.
/// `None` ⇒ every rung refused, or the platform refused async mode — either way the session
/// has no video.
///
/// `configure()` can succeed and `start()` still fail: start is where the codec allocates its
/// buffers from the output consumer. A codec that failed `start` cannot be reconfigured, so
/// each rung builds a fresh one. A codec can also start and then hang a few frames in, every
/// time; [`RungFloor`] raises `floor` for that. The rungs shed, most-suspect first:
///
///   1. The ASC reader's `COMPOSER_OVERLAY` usage — overlay + GPU-sampled + vendor-vdec in one
///      allocation is what an old OMX-era gralloc refuses (see [`AscSink::create`]).
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
    floor: usize,
) -> Option<Started> {
    let mode = client.mode();
    let mime = codec_mime(client.video_format().codec);
    // Fetched ONCE, ahead of the ladder, so a retry rung never pays the wait again.
    let hdr_static = hdr_static(client);
    let priority = sink::priority(opts);
    // The layer's colour tag; `None` on Android 10 and 11 for a studio-range session, which
    // then presents through the SurfaceView (its decoder tags its own buffers).
    let sdk = crate::sys::sdk_int();
    let dataspace = layer_dataspace(&client.video_format().color, sdk);
    let selected = asc_backend_selected(opts.chromeos);
    let asc_wanted = selected && dataspace.is_some();
    if !asc_wanted {
        log::info!(
            "decode: present backend = SurfaceView ({})",
            if !selected && opts.chromeos {
                "ChromeOS".to_string()
            } else if !selected {
                "present_backend sysprop".to_string()
            } else {
                format!("Android {sdk} has no layer dataspace for a studio-range session")
            }
        );
    }
    let rungs = bring_up_rungs(asc_wanted, opts.low_latency_mode);
    // The ladder is never empty; the sysprop can shorten it under a floor already raised.
    let floor = floor.min(rungs.len() - 1);
    for (rung, &(backend, keys)) in rungs.iter().enumerate().skip(floor) {
        if rung > floor {
            log::warn!(
                "decode: decoder refused that configuration — retrying through {} with {}",
                backend_label(backend),
                keys_label(keys)
            );
        }
        let Some(mut codec) = create_codec(mime, opts.decoder_for(mime)) else {
            log::error!("decode: no {mime} decoder on this device");
            return None;
        };
        // The decoder's *actual* resolved name (Kotlin's pick, or the platform default when it
        // fell back) drives both the HUD label and which vendor low-latency keys apply.
        let codec_name = codec.name().unwrap_or_default();
        if rung == floor {
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
        // SurfaceView presenter covers API < 29, ChromeOS, an ASC init failure, the sysprop, or a
        // rung that dropped it. A non-null `asc` means the codec renders into the reader, not the
        // SurfaceView window.
        let asc = backend.and_then(|overlay| {
            // The negotiated colour is authoritative (PQ vs HLG, range) — not a guess the codec's
            // output format later corrects; many decoders never echo `color-transfer` at all.
            AscSink::create(
                window,
                mode.width as i32,
                mode.height as i32,
                opts.surface_size.clone(),
                opts.src_crop.clone(),
                opts.layers.clone(),
                opts.panel_hz,
                dataspace.unwrap_or(0), // `Some` whenever `asc_wanted`
                mode.refresh_hz,
                priority,
                overlay,
                opts.force_rate_switch(),
            )
            .map(|mut a| {
                a.set_hdr_meta(hdr_static);
                a.wake_on_image(ev_tx.clone());
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
        // `asc.as_ref().and(backend)`, not `backend`: an ASC rung whose backend failed to
        // CREATE fell back to the SurfaceView within the rung, and the label names what runs.
        let backend = backend_label(asc.as_ref().and(backend));
        log::info!(
            "decode: decoder started (async) at {}x{} through {backend} with {} (rung {rung})",
            mode.width,
            mode.height,
            keys_label(keys)
        );
        let backends: Vec<Option<bool>> = rungs.iter().map(|r| r.0).collect();
        let lower_rung = rung_after_failure(&backends, rung, asc.is_some());
        return Some(Started {
            codec,
            asc,
            lower_rung,
            rung,
            backend,
        });
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
pub(crate) fn start_hint(
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
/// feeder fills, the SurfaceView window the codec renders into under that backend, and where on
/// the bring-up ladder this codec runs.
pub(super) struct Ctx {
    pub(super) codec: MediaCodec,
    pub(super) client: Arc<NativeClient>,
    pub(super) stats: Arc<crate::stats::VideoStats>,
    /// The adaptive-bitrate controller wants the `decode` stage as its decoder-backlog signal:
    /// then `in_flight` is fed regardless of the HUD.
    measure_decode: bool,
    in_flight: Arc<Mutex<VecDeque<(u64, i128)>>>,
    clock_offset: Arc<AtomicI64>,
    /// The cell the audio plane steers its jitter ring by — the present path is the only point
    /// that knows when a frame actually reached glass; both backends publish into it.
    pub(super) video_e2e: Arc<AtomicU64>,
    pub(super) meter: Arc<PresentMeter>,
    pub(super) tracker: Arc<DisplayTracker>,
    pub(super) window: NativeWindow,
    /// The persistent Sender each ASC transaction's completion callback rides back on.
    pub(super) present_tx: Option<mpsc::Sender<DecodeEvent>>,
    /// The session's decoded-size cell, refreshed on each output-format change.
    decoded_size: Arc<AtomicU64>,
    /// The bring-up ladder rung this codec's replacement starts at, if there is one.
    lower_rung: Option<usize>,
    /// The rung this codec runs on, and its present backend's [`backend_label`].
    rung: usize,
    backend: &'static str,
}

impl Ctx {
    pub(super) fn offset(&self) -> i64 {
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
    /// An AU was lost this pass, on the wire or here: its second is not unbroken.
    lost: bool,
    /// The codec freed an input slot this pass (the hung-codec check, see [`InputStall`]).
    input_offered: bool,
    /// An AU went into the codec this pass (the silent-codec check, see [`OutputSilence`]).
    fed_any: bool,
    /// The receiver rule asked for a keyframe this pass.
    ask_keyframe: bool,
    /// ASurfaceControl transaction completions, applied after the drain (on the decode thread,
    /// not the binder thread that posted them).
    present_completes: Vec<PresentComplete>,
}

/// The loop's working state: every value a pass mutates.
struct State {
    sink: Box<dyn Sink>,
    free_inputs: VecDeque<usize>,
    pending_aus: VecDeque<Frame>,
    ready: Vec<OutputReady>,
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
    /// Core's freeze arm count at the last cadence re-anchor ([`NativeClient::recovery_arms`]).
    last_arms: u64,
    /// Each AU's wire flags from `dispatch` to `present`, keyed by the codec-echoed pts: what
    /// core's freeze folds per decoded output.
    recovery_flags: VecDeque<(u64, u32)>,
    fatal: bool,
    /// The codec stopped taking input, or went silent after a feed: this run ends and the codec
    /// is rebuilt.
    wedged: bool,
    /// The sink's `ShowsNothing` verdict, for `housekeeping` to end the run on.
    shows_nothing: bool,
    stall: InputStall,
    silence: OutputSilence,
    /// The codec handing back no buffer for a slot it offered, or failing a queue call: it left
    /// the executing state, whether or not its error callback ran. A piece going in clears it.
    refused: InputStall,
    backstops: Backstops,
    tally: DecodeTally,
    /// Which AUs reach the codec: the AV1 receiver rule and a rebuilt run's keyframe wait.
    intake: Intake,
    /// An AU was lost, here or on the wire, and no keyframe has been fed since.
    loss_open: bool,
    /// The codec was fed past a loss or reported an error: a failure after this may be the
    /// stream's, not the configuration's ([`RungFloor`]).
    unclean: bool,
    /// The visible picture the newest AV1 keyframe's render_size states. A decoder that ignores
    /// it hands over the coded size, padding rows included.
    av1_render: Option<(i32, i32)>,
    /// The colour the newest AV1 sequence header states. It outranks the codec's report, which
    /// reads BT.709 until a decoder writes its own.
    stream_colour: Option<ColorInfo>,
    /// The first frame of a new format: this run ends and the next codec takes it.
    reformat: Option<Frame>,
}

/// The 1 Hz `pf.decode` line: what went into the codec and what came back, per second.
struct DecodeTally {
    since: Instant,
    out: u64,
    slots: u64,
    /// The fed and withheld totals at `since`.
    fed: u64,
    withheld: u64,
}

impl DecodeTally {
    fn new() -> DecodeTally {
        DecodeTally {
            since: Instant::now(),
            out: 0,
            slots: 0,
            fed: 0,
            withheld: 0,
        }
    }

    /// Once a second: logs the window when the codec was fed or answered, then opens the next.
    /// `fed` and `withheld` are running totals, `parked` the park's depth now.
    fn flush(&mut self, ctx: &Ctx, fed: u64, withheld: u64, parked: usize) {
        let now = Instant::now();
        if now.duration_since(self.since) < Duration::from_secs(1) {
            return;
        }
        let fed_now = fed - self.fed;
        if fed_now > 0 || self.out > 0 || self.slots > 0 {
            log::info!(
                target: "pf.decode",
                "fed={fed_now} out={} slots={} parked={parked} withheld={} rung={} backend={}",
                self.out,
                self.slots,
                withheld - self.withheld,
                ctx.rung,
                ctx.backend
            );
        }
        *self = DecodeTally {
            since: now,
            out: 0,
            slots: 0,
            fed,
            withheld,
        };
    }
}

/// The receiver rule's decoder class for this session ([`Intake`]): AV1 only, strict on the
/// Tensor G5 decoder, lenient on the rest. `debug.punktfunk.au_admission` = `strict` /
/// `lenient` / `off` overrides the pick.
fn admission(codec: u8, decoder: &str) -> Option<DecoderClass> {
    if codec != punktfunk_core::quic::CODEC_AV1 {
        return None;
    }
    let class = match sysprop(c"debug.punktfunk.au_admission").as_deref() {
        Some("off") => return None,
        Some("strict") => DecoderClass::Strict,
        Some("lenient") => DecoderClass::Lenient,
        _ if decoder == "c2.google.av1.decoder" => DecoderClass::Strict,
        _ => DecoderClass::Lenient,
    };
    log::info!("decode: AV1 AU admission {class:?} for {decoder}");
    Some(class)
}

/// One feeder delivery as the intake rules see it.
fn delivery(f: &Frame) -> Delivery {
    Delivery {
        index: f.frame_index,
        first: f.part.is_none_or(|p| p.first),
        flags: f.flags,
    }
}

/// Fold one decoded output into core's freeze: `true` shows it. MediaCodec flags no keyframe
/// and parses nothing, so the AU's wire flags carry it all.
fn shows(client: &NativeClient, flags: u32) -> bool {
    !client
        .recovery_on_decoded(Decoded {
            flags,
            keyframe: false,
            evidence: AnchorEvidence::Unavailable,
            local: LocalRecovery::NONE,
            decode_order: None,
        })
        .hold
}

impl State {
    fn new(sink: Box<dyn Sink>, intake: Intake) -> State {
        State {
            sink,
            free_inputs: VecDeque::new(),
            pending_aus: VecDeque::new(),
            ready: Vec::new(),
            fed: 0,
            rendered: 0,
            discarded: 0,
            oversized_dropped: 0,
            part_open: None,
            queued_stamps: VecDeque::new(),
            last_arms: 0,
            recovery_flags: VecDeque::new(),
            fatal: false,
            wedged: false,
            shows_nothing: false,
            stall: InputStall::default(),
            silence: OutputSilence::default(),
            refused: InputStall::default(),
            backstops: Backstops::new(),
            tally: DecodeTally::new(),
            intake,
            loss_open: false,
            unclean: false,
            av1_render: None,
            stream_colour: None,
            reformat: None,
        }
    }

    /// Route one [`DecodeEvent`] into the working sets.
    fn dispatch(&mut self, client: &NativeClient, ev: DecodeEvent, pass: &mut Pass) {
        match ev {
            DecodeEvent::Au(f, gap) => {
                if f.new_format.is_some() {
                    self.reformat = Some(f);
                    return;
                }
                if gap > 0 {
                    self.loss_open = true;
                    pass.lost = true;
                }
                let verdict = self.intake.note(delivery(&f), gap);
                pass.ask_keyframe |= verdict.ask_keyframe;
                if !verdict.feed {
                    return;
                }
                // One entry per AU (parts share the pts): the completing delivery carries it.
                if f.complete {
                    self.recovery_flags.push_back((f.pts_ns / 1000, f.flags));
                    if self.recovery_flags.len() > IN_FLIGHT_CAP {
                        self.recovery_flags.pop_front();
                    }
                }
                self.pending_aus.push_back(f);
                if self.pending_aus.len() > FRAME_PARK_CAP {
                    self.pending_aus.pop_front(); // sustained overflow — drop oldest
                    pass.aus_dropped += 1;
                    pass.lost = true;
                    self.loss_open = true;
                    self.local_loss(pass);
                }
            }
            DecodeEvent::InputAvailable(i) => {
                self.free_inputs.push_back(i);
                pass.input_offered = true;
                self.tally.slots += 1;
            }
            DecodeEvent::OutputAvailable {
                index,
                pts_us,
                decoded_ns,
                decoded_mono_ns,
            } => {
                self.ready.push(OutputReady {
                    index,
                    pts_us,
                    decoded_ns,
                    decoded_mono_ns,
                });
                self.tally.out += 1;
            }
            DecodeEvent::FormatChanged => pass.fmt_dirty = true,
            DecodeEvent::Vsync => pass.vsync_tick = true,
            DecodeEvent::ImageAvailable => {}
            DecodeEvent::Error { fatal: true } => self.fatal = true,
            // A recoverable or transient codec error is a decode hiccup on a broken reference
            // chain: core holds the concealed output it recovers into off the screen.
            DecodeEvent::Error { fatal: false } => {
                client.recovery_on_codec_error();
                self.unclean = true;
            }
            DecodeEvent::PresentComplete(pc) => pass.present_completes.push(pc),
        }
    }

    /// This loop dropped an AU the rule had admitted, and the AUs behind it may name it. The
    /// parked ones are judged again and the next arrival sees the gap, as for a network loss.
    fn local_loss(&mut self, pass: &mut Pass) {
        if !self.intake.local_loss() {
            return;
        }
        for f in std::mem::take(&mut self.pending_aus) {
            let verdict = self.intake.rejudge(delivery(&f));
            pass.ask_keyframe |= verdict.ask_keyframe;
            if verdict.feed {
                self.pending_aus.push_back(f);
            }
        }
    }

    /// `feed` dropped an admitted AU: core freezes and asks for a keyframe, and the parked AUs
    /// behind it are judged again ([`State::local_loss`]).
    fn drop_admitted(&mut self, client: &NativeClient, pass: &mut Pass) {
        client.recovery_lost();
        self.loss_open = true;
        pass.lost = true;
        self.local_loss(pass);
    }

    /// The pass proper, after the drain: completions, the vsync tick, the format change, feeding,
    /// the re-anchor cadence reset, then presenting.
    fn apply(&mut self, ctx: &Ctx, pass: &mut Pass) {
        let off = ctx.offset();
        for pc in pass.present_completes.drain(..) {
            self.sink.on_present_complete(pc, off, ctx);
        }
        if pass.vsync_tick {
            self.sink.on_vsync(ctx);
        }
        ctx.stats.note_skipped_overflow(pass.aus_dropped); // parked-AU overflow: skips, flagged as such
        if pass.fmt_dirty {
            // Only the ASurfaceControl sink crops; the SurfaceView shows what the codec hands it.
            let picture =
                super::display::picture_size(&ctx.codec).map(|(w, h)| match self.av1_render {
                    Some((rw, rh)) if self.sink.is_asc() => (w.min(rw), h.min(rh)),
                    _ => (w, h),
                });
            if let Some((w, h)) = picture {
                ctx.decoded_size
                    .store(crate::session::pack_surface_size(w, h), Ordering::Relaxed);
            }
            self.sink
                .on_format_changed(ctx, picture, self.stream_colour);
        }
        self.feed(ctx, pass);
        // The cadence loop's re-anchor seam. An arm is a loss: the frames past it come through
        // a decoder that just recovered, so the source→presentable delay the loop measured no
        // longer holds. Core's arm count catches every site, its own tick included.
        let arms = ctx.client.recovery_arms();
        if arms != self.last_arms {
            self.last_arms = arms;
            self.sink.reset_cadence();
        }
        self.present(ctx);
    }

    /// Queue as many parked AUs as there are free input buffer slots (async mode: the indices
    /// come from `InputAvailable` callbacks, not a dequeue). An AU larger than the buffer is
    /// DROPPED: a truncated AU is corrupt input the decoder chews on silently, poisoning the
    /// reference chain.
    ///
    /// Slice-progressive deliveries ([`Frame::part`]) feed as they arrive: every piece rides
    /// [`BUFFER_FLAG_PARTIAL_FRAME`] except the AU's last, all at the AU's pts. `part_open` is the
    /// continuity ledger; [`crate::feed_rules`] judges each piece against it and says what one
    /// that cannot go in leaves behind. A slot whose queue call did not take stays in
    /// `free_inputs`: the codec offers a slot once, so a forgotten one is gone until a rebuild.
    fn feed(&mut self, ctx: &Ctx, pass: &mut Pass) {
        let codec = &ctx.codec;
        while !self.pending_aus.is_empty() && !self.free_inputs.is_empty() {
            let idx = self.free_inputs.pop_front().unwrap();
            let frame = self.pending_aus.pop_front().unwrap();
            let piece = Piece::of(&frame);
            match gate(&mut self.part_open, &piece) {
                Gate::Go => {}
                Gate::Close(open) => {
                    // Spend THIS slot on the close; the current frame re-queues for the next one.
                    if let Err(e) = codec.queue_input_buffer_by_index(idx, 0, 0, open.pts_us, 0) {
                        self.free_inputs.push_front(idx);
                        log::warn!("decode: close of abandoned partial AU {}: {e}", open.index);
                    }
                    log::warn!(
                        "decode: partial AU {} abandoned mid-feed — closed empty, requesting keyframe",
                        open.index
                    );
                    // The close makes the codec emit concealed garbage at the dead pts — freeze it
                    // off the glass until the recovery keyframe re-anchors.
                    self.pending_aus.push_front(frame);
                    self.drop_admitted(&ctx.client, pass);
                    continue;
                }
                // The head was lost upstream: discard and re-sync at the next `first`. No
                // `local_loss`: the gap or local drop that lost the head already told the rule.
                Gate::Orphan => {
                    self.free_inputs.push_front(idx);
                    ctx.client.recovery_lost();
                    continue;
                }
            }
            let Some(dst) = codec.input_buffer(idx) else {
                self.refuse(idx, frame);
                break;
            };
            let au = &frame.data;
            if au.len() > dst.len() {
                self.oversized_dropped += 1;
                log::warn!(
                    "decode: AU {} > input buffer {} — dropped ({} so far), requesting keyframe",
                    au.len(),
                    dst.len(),
                    self.oversized_dropped
                );
                self.spoil(ctx, pass, idx, &piece, Cause::Oversize);
                continue;
            }
            let n = au.len();
            // SAFETY: `au` (wire AU) and `dst` (codec input buffer) are distinct allocations, both
            // valid for `n` bytes; `MaybeUninit<u8>` is layout-identical to `u8`, so this
            // initializes dst[..n].
            unsafe {
                std::ptr::copy_nonoverlapping(au.as_ptr(), dst.as_mut_ptr().cast::<u8>(), n);
            }
            let flags = if piece.last {
                0
            } else {
                BUFFER_FLAG_PARTIAL_FRAME
            };
            if let Err(e) = codec.queue_input_buffer_by_index(idx, 0, n, piece.pts_us, flags) {
                log::warn!("decode: queue_input_buffer_by_index: {e}");
                self.refusing();
                self.spoil(ctx, pass, idx, &piece, Cause::Refused);
                continue;
            }
            self.refused = InputStall::default();
            self.note_queued(ctx, &frame, &piece, pass);
        }
    }

    /// The codec handed back no buffer for a slot it offered, and takes nothing else this pass.
    /// Slot and AU both stay ours: forgetting the slot leaks a codec buffer, dropping the AU
    /// holes the reference chain.
    fn refuse(&mut self, idx: usize, frame: Frame) {
        self.refusing();
        self.free_inputs.push_front(idx);
        self.pending_aus.push_front(frame);
    }

    /// The codec refused again, a no-buffer slot or a failed queue call. Refusing for the hang
    /// patience with no piece accepted between, the codec is gone and is rebuilt.
    fn refusing(&mut self) {
        if self.refused.poll(false, true, Instant::now()) {
            log::warn!(
                "decode: the codec refused input for {} ms — treating it as hung",
                INPUT_STALL_PATIENCE.as_millis()
            );
            self.wedged = true;
        }
    }

    /// A piece that cannot go in leaves the ledger poisoned, or its AU lost
    /// ([`crate::feed_rules::spoiled`]). Its slot was never taken: it stays ours for the next AU.
    fn spoil(&mut self, ctx: &Ctx, pass: &mut Pass, idx: usize, piece: &Piece, cause: Cause) {
        self.free_inputs.push_front(idx);
        match spoiled(piece, cause) {
            Spoiled::Poison { ledger, freeze_now } => {
                if freeze_now {
                    ctx.client.recovery_lost();
                }
                self.part_open = Some(ledger);
            }
            Spoiled::Lost => self.drop_admitted(&ctx.client, pass),
        }
    }

    /// `piece` of `frame` went into the codec: what a keyframe head resets, the fed counters and
    /// the ledger.
    fn note_queued(&mut self, ctx: &Ctx, frame: &Frame, piece: &Piece, pass: &mut Pass) {
        pass.fed_any = true;
        // A keyframe needs no reference. Anything else fed after a loss may name the lost AU.
        if piece.keyframe {
            self.loss_open = false;
            let av1 = ctx.client.video_format().codec == punktfunk_core::quic::CODEC_AV1;
            self.av1_render = render_after(self.av1_render, av1, || {
                pf_bitstream::av1::render_size(&frame.data)
            });
            let colour = colour_after(self.stream_colour, av1, || {
                pf_bitstream::av1::sequence_info(&frame.data).map(|s| ColorInfo {
                    primaries: s.color_primaries,
                    transfer: s.transfer_characteristics,
                    matrix: s.matrix_coefficients,
                    full_range: u8::from(s.full_range),
                })
            });
            if colour != self.stream_colour {
                self.stream_colour = colour;
                // A flip re-tags the frames still in the decoder a few refreshes early.
                self.sink.on_format_changed(ctx, None, colour);
            }
        }
        self.unclean |= self.loss_open;
        // `fed` counts ACCESS UNITS toward the HUD's fed/decoded balance, so the closing piece
        // (or a whole AU) bumps it. The queued stamp marks the same instant: the P3 decode split
        // measures `codec` from here, so a slice-progressive head start shows as codec-pure shrink.
        if piece.last {
            self.fed += 1;
            self.queued_stamps
                .push_back((piece.pts_us, now_realtime_ns()));
            if self.queued_stamps.len() > IN_FLIGHT_CAP {
                self.queued_stamps.pop_front(); // stale — codec never echoed it back
            }
        }
        self.part_open = piece.queued(frame.data.len());
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
            let queued = take_by_pts(&mut self.queued_stamps, o.pts_us);
            let codec_us = queued.map(|q| ((o.decoded_ns - q).max(0) / 1000) as u64);
            let feed_us = match (queued, received_ns) {
                (Some(q), Some(r)) => Some(((q - r).max(0) / 1000) as u64),
                _ => None,
            };
            if let Some(m) = meter {
                // Same formula + clamp as the HUD's capture→decoded headline in `note_decoded_pts`.
                let e2e_ns = o.decoded_ns + clock_offset as i128 - o.pts_us as i128 * 1000;
                m.note_decode(feed_us, codec_us, crate::stats::sane_us(e2e_ns));
            }
        }
    }

    /// Route the ready outputs toward glass, recording each one's decode-split + e2e first. The
    /// sink folds EVERY output through core's freeze ([`shows`]) in pts (== decode) order, even
    /// the ones newest-wins discards, so the two-mark re-anchor count stays correct; a held one
    /// is concealment and never reaches glass. `ready` is drained.
    fn present(&mut self, ctx: &Ctx) {
        if self.ready.is_empty() {
            return;
        }
        // The ASC sink keeps its own 1 Hz line, so only the SurfaceView path feeds the meter.
        let meter = (!self.sink.is_asc()).then_some(&*ctx.meter);
        self.note_decode_stage(ctx, meter);
        let ready = std::mem::take(&mut self.ready);
        let flags = &mut self.recovery_flags;
        let routed = self.sink.on_outputs(ctx, ready, &mut |pts_us| {
            shows(&ctx.client, take_flags(flags, pts_us))
        });
        self.rendered += routed.rendered;
        self.discarded += routed.discarded;
        ctx.stats.note_skipped(routed.skipped); // HUD `skipped` counter (newest-wins + held-off drops)
    }

    /// The sink's decision point, run EVERY pass — frame arrivals, vsync ticks and the 5 ms
    /// housekeeping wake all land here, which is what reopens the glass budget on time even when
    /// the choreographer clock is absent. `lost`: this pass lost an AU. Keeps a `ShowsNothing`
    /// verdict for `housekeeping`.
    fn pump(&mut self, ctx: &Ctx, clock: Option<&VsyncShared>, lost: bool) {
        if self.sink.pump(ctx, clock) {
            self.rendered += 1;
        }
        if self.sink.flush(ctx, clock, lost) == Some(Verdict::ShowsNothing) {
            self.shows_nothing = true;
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

    /// The hung-codec checks ([`InputStall`], [`OutputSilence`]) and the sink's `ShowsNothing`
    /// verdict, each a wedge; the backstops ([`Backstops::poll`]), the receiver rule's keyframe
    /// ask, then the 1 Hz `pf.decode` line, once a pass; core throttles the ask. Evaluated after
    /// `feed`, so an AU that arrived this pass has either been fed or is parked in
    /// `pending_aus`. `had_output`: an output arrived.
    fn housekeeping(&mut self, ctx: &Ctx, had_output: bool, pass: &Pass) {
        let waiting = !self.pending_aus.is_empty();
        let now = Instant::now();
        if self.stall.poll(pass.input_offered, waiting, now) {
            log::warn!(
                "decode: the codec took no input for {} ms with {} AU(s) waiting — treating it as hung",
                INPUT_STALL_PATIENCE.as_millis(),
                self.pending_aus.len()
            );
            self.wedged = true;
        }
        if self
            .silence
            .poll(pass.fed_any, pass.input_offered || had_output, now)
        {
            log::warn!(
                "decode: an AU went in {} ms ago and neither an output nor an input slot came \
                 back — treating it as hung",
                INPUT_STALL_PATIENCE.as_millis()
            );
            self.wedged = true;
        }
        if std::mem::take(&mut self.shows_nothing) {
            log::warn!(
                "presenter: verdict shows nothing for 2 s — treating the configuration as failed"
            );
            self.wedged = true;
        }
        self.backstops.poll(
            &ctx.client,
            self.fed + self.intake.withheld(),
            had_output,
            waiting,
            pass.aus_dropped,
        );
        if pass.ask_keyframe {
            let _ = ctx.client.request_keyframe();
        }
        self.tally.flush(
            ctx,
            self.fed,
            self.intake.withheld(),
            self.pending_aus.len(),
        );
    }
}

/// The `pf-decode-feed` thread ([`feeder_loop`]): it blocks on the network so the loop doesn't,
/// and an AU's arrival becomes an event that wakes the loop at once. `stop` ends it with this
/// run; the session's `shutdown` would end every later run too.
struct Feeder {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Feeder {
    fn spawn(
        client: &Arc<NativeClient>,
        stats: &Arc<crate::stats::VideoStats>,
        measure_decode: bool,
        in_flight: Arc<Mutex<VecDeque<(u64, i128)>>>,
        ev_tx: mpsc::Sender<DecodeEvent>,
        last_gap_ns: Arc<AtomicI64>,
    ) -> Feeder {
        let stop = Arc::new(AtomicBool::new(false));
        let join = {
            let client = client.clone();
            let stats = stats.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("pf-decode-feed".into())
                .spawn(move || {
                    feeder_loop(
                        client,
                        stats,
                        measure_decode,
                        in_flight,
                        stop,
                        ev_tx,
                        last_gap_ns,
                    );
                })
                .ok()
        };
        Feeder { stop, join }
    }

    fn stop_and_join(self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join {
            let _ = j.join();
        }
    }
}

/// The `pf-decode-feed` thread: block on the connector for the next access unit so the async loop
/// never has to. Drops what core refuses, stamps `last_gap_ns` on a loss gap, stamps receipt for
/// the decode stage (the connector already noted it for the overlay), then hands the AU to the
/// loop via the event channel. Registers with ADPF: a stall here is a late frame. Exits when
/// `shutdown` is set, the session closes, or the loop's receiver is gone.
fn feeder_loop(
    client: Arc<NativeClient>,
    stats: Arc<crate::stats::VideoStats>,
    measure_decode: bool,
    in_flight: Arc<Mutex<VecDeque<(u64, i128)>>>,
    shutdown: Arc<AtomicBool>,
    ev_tx: mpsc::Sender<DecodeEvent>,
    last_gap_ns: Arc<AtomicI64>,
) {
    client.register_hot_thread();
    let mut refused = None;
    while !shutdown.load(Ordering::Relaxed) {
        match client.next_frame(Duration::from_millis(5)) {
            Ok(frame) => {
                // Reported here so core asks for a lost range the moment it shows. Parts repeat
                // their AU's index: once, on the first. A refused straggler would rewind the
                // decoder's references; it never reaches the loop and is no loss.
                let arrival = frame
                    .part
                    .is_none_or(|p| p.first)
                    .then(|| client.recovery_on_au(frame.frame_index, frame.complete, 0));
                if !hand_on(&mut refused, frame.frame_index, arrival.map(|a| a.admit)) {
                    continue;
                }
                let gap = arrival.map_or(0, |a| a.gap);
                if gap > 0 {
                    last_gap_ns.store(now_monotonic_ns(), Ordering::Relaxed);
                }
                // Park the receipt stamp whenever the `decode` stage is consumed: the HUD, or the
                // ABR decode signal (`measure_decode`).
                if (stats.enabled() || measure_decode) && frame.complete {
                    let received_ns = note_received_frame(&client, &frame);
                    let mut g = in_flight
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    g.push_back((frame.pts_ns / 1000, received_ns));
                    if g.len() > IN_FLIGHT_CAP {
                        g.pop_front(); // stale — codec never echoed it back
                    }
                }
                // A new format ends this codec: what follows waits in core for the next one's
                // feeder.
                let reformat = frame.new_format.is_some();
                if ev_tx.send(DecodeEvent::Au(frame, gap)).is_err() || reformat {
                    break; // the decode loop is gone, or is ending for the new format
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
