//! WASAPI loopback capture of the desktop mix (Windows analogue of the PipeWire sink-monitor).
//! Interleaved f32 PCM at the opened engine rate — never above it; see
//! [`WasapiLoopbackCapturer::opened_rate`] — in the requested layout (stereo / 5.1 / 7.1,
//! `dwChannelMask` FL FR FC LFE RL RR SL SR). Shared-mode autoconvert does SRC and up/downmix;
//! a silent sink is reshaped to the session's layout so apps render surround into it.
//! WASAPI objects are COM-apartment-bound and `!Send`, so they live on a dedicated thread;
//! the struct holds only the channel, stop flag, and join handle.
//!
//! Capture binds the wiring plan's loopback endpoint explicitly, never the current default
//! (that races the plan's own `IPolicyConfig` write). A 1 s watchdog follows a capturable
//! default change and snaps a known-dud back to the plan. Device errors reopen with capped
//! exponential backoff cut short by an endpoint-set change. A plan with no loopback endpoint
//! is never retried: [`wiring_plan::plan`](super::wiring_plan) is pure in the set, so the
//! thread parks on a fingerprint poll until the set moves. On drop, parked default playback
//! and recording devices are restored — both are session-scoped.
//!
//! Pin: `design/hi-res-audio.md`, [`super::wiring_plan`], [`super::audio_control`].

use super::capture_policy::{CaptureStats, FightDamper, FIGHT_BACKOFF};
use super::{audio_control, voice_route, wiring_plan, AudioCapturer, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use wasapi::{Device, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

pub struct WasapiLoopbackCapturer {
    chunks: Receiver<Vec<f32>>,
    channels: u32,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// Shared with the capture thread so drops mean "encode fell behind", not "nobody is reading".
    /// Native/gamestream planes park a capturer between sessions ([`idle`](AudioCapturer::idle))
    /// on a bounded channel; without this the thread fills it once and then warns that the
    /// stream will click with no stream to click.
    active: Arc<AtomicBool>,
    /// Rate the endpoint actually opened at. Written before the thread reports ready; read by
    /// [`AudioCapturer::sample_rate`]. Shared-mode autoconvert succeeds on an upward request
    /// with interpolated samples (`design/hi-res-audio.md`), so the open declines instead of
    /// padding and stores what it settled for.
    opened_rate: Arc<AtomicU32>,
}

impl WasapiLoopbackCapturer {
    pub fn open(channels: u32, rate_hz: u32) -> Result<WasapiLoopbackCapturer> {
        anyhow::ensure!(
            matches!(channels, 2 | 6 | 8),
            "WASAPI loopback backend supports 2/6/8 channels (got {channels})"
        );
        anyhow::ensure!(rate_hz > 0, "audio capture rate must be positive");
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let active = Arc::new(AtomicBool::new(true));
        let active_t = active.clone();
        // Honest until the endpoint is read; also the final answer on the common 48 kHz path.
        let opened_rate = Arc::new(AtomicU32::new(rate_hz));
        let opened_rate_t = opened_rate.clone();
        // 30 s: first open may auto-install the Steam Streaming pair (two driver installs, ~5 s each).
        let ((), join) = crate::ready::spawn_ready(
            "punktfunk-wasapi-audio",
            Duration::from_secs(30),
            move |ready| {
                if let Err(e) = capture_thread(
                    tx,
                    stop_t,
                    ready,
                    channels,
                    rate_hz,
                    active_t,
                    opened_rate_t,
                ) {
                    tracing::error!(error = %format!("{e:#}"), "wasapi loopback thread failed");
                }
            },
            |join| {
                // Unstopped, it captures for the process lifetime with the playback default parked.
                stop.store(true, Ordering::SeqCst);
                crate::ready::reap_timed_out("wasapi loopback", join)
            },
        )?;
        // Settled rate, not `rate_hz` — a log must not print one rate while the stream carries another.
        tracing::info!(
            channels,
            rate_hz = opened_rate.load(Ordering::Relaxed),
            "WASAPI loopback capture: f32"
        );
        Ok(WasapiLoopbackCapturer {
            chunks: rx,
            channels,
            stop,
            join: Some(join),
            active,
            opened_rate,
        })
    }
}

impl Drop for WasapiLoopbackCapturer {
    fn drop(&mut self) {
        // Receiver dies with us; leftover pushes must not count as encode lag.
        self.active.store(false, Ordering::Relaxed);
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl AudioCapturer for WasapiLoopbackCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        self.next_chunk_within(Duration::from_secs(5))
    }
    fn next_chunk_within(&mut self, budget: Duration) -> Result<Vec<f32>> {
        match self.chunks.recv_timeout(budget) {
            Ok(c) => Ok(c),
            // Quiet sink is not a failure — empty chunk keeps the capturer. Dead thread is Err.
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("wasapi audio thread ended")),
        }
    }
    fn channels(&self) -> u32 {
        self.channels
    }
    fn sample_rate(&self) -> u32 {
        self.opened_rate.load(Ordering::Relaxed)
    }
    fn drain(&mut self) {
        while self.chunks.try_recv().is_ok() {}
        // After the drain so the capture thread never counts a drop this call is emptying.
        self.active.store(true, Ordering::Relaxed);
    }
    fn idle(&mut self) {
        // Channel will fill and stay full; those drops are not encode lag. See `active`.
        self.active.store(false, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetMode {
    /// Plan's loopback endpoint; parks default playback on it (client-only when the sink is silent).
    Assert,
    /// Current default render — operator changed it mid-stream, or `PUNKTFUNK_KEEP_DEFAULT`.
    Follow,
}

enum Next {
    Stopped,
    Reopen(TargetMode),
}

/// `(bind_plan, assert_plan)`: which endpoint this open captures, and whether it may park the
/// playback default on it.
///
/// `keep_default` normally means both — capture whatever the operator chose and write nothing.
/// A seat splits them: its planned endpoint is its own minted sink, while the box's defaults are
/// shared with every other seat, so it binds the plan and still writes nothing.
fn binding(mode: TargetMode, keep_default: bool, seat: bool) -> (bool, bool) {
    let assert = mode == TargetMode::Assert;
    (assert && (!keep_default || seat), assert && !keep_default)
}

/// First reopen wait after a transient failure. Doubles per miss up to [`REOPEN_BACKOFF_CAP`];
/// resets after an attempt that lived [`REOPEN_STABLE_AFTER`] or an endpoint-set change. Do not
/// retry flat at 2 s — each attempt re-runs the wiring pass, IPolicyConfig included.
const REOPEN_BACKOFF_START: Duration = Duration::from_secs(2);
const REOPEN_BACKOFF_CAP: Duration = Duration::from_secs(60);
/// An attempt this old streamed: its death is a new failure, not the next miss of a failing open.
const REOPEN_STABLE_AFTER: Duration = Duration::from_secs(5);
/// Fingerprint poll while backing off or waiting out an unsatisfiable plan: enumerate-and-hash
/// only. A change ends the wait immediately so a re-arrived endpoint is not stuck behind the cap.
const ENDPOINT_POLL_EVERY: Duration = Duration::from_secs(2);
const DEFAULT_CHECK_EVERY: Duration = Duration::from_secs(1);
/// First-open tries before the handshake surfaces Err. Session start races virtual-display
/// attach and this module's own IPolicyConfig flips; activate then fails with 0x80070002
/// (endpoint mid-re-registration).
const FIRST_OPEN_ATTEMPTS: u32 = 3;
/// Endpoint churn settles in well under a second.
const FIRST_OPEN_RETRY_PAUSE: Duration = Duration::from_secs(1);
/// Live loopback captures. A join session opens a second one on the same sink, and the
/// parked defaults, the sink's layout and the voice pins are shared: only the last capture
/// to end puts them back, and only a capture alone on the sink reshapes it.
static LIVE_CAPTURES: Mutex<usize> = Mutex::new(0);

fn live_captures() -> std::sync::MutexGuard<'static, usize> {
    LIVE_CAPTURES.lock().unwrap_or_else(|e| e.into_inner())
}

/// A loopback capture is running, so the parked defaults are its to manage.
pub fn capture_live() -> bool {
    *live_captures() > 0
}

/// One [`LIVE_CAPTURES`] slot, with the voice pins it owns. Drop runs on every exit, a panic
/// included: a leaked count would keep the parked defaults until the host restarts.
struct LiveCapture {
    /// Outlives every reopen: the pins stay while the capture re-plans, and go at the end.
    voice: voice_route::VoiceRoute,
}

impl LiveCapture {
    fn new() -> LiveCapture {
        *live_captures() += 1;
        LiveCapture {
            voice: voice_route::VoiceRoute::default(),
        }
    }
}

impl Drop for LiveCapture {
    /// Last capture out: voice apps back first, then both parked defaults (no-op if never
    /// parked, or if the operator moved them), then the sink's speaker layout. Held across the
    /// restore so a capture starting now parks after it, not before.
    fn drop(&mut self) {
        let mut live = live_captures();
        *live -= 1;
        if *live == 0 {
            self.voice.clear();
            audio_control::restore_default_playback();
            audio_control::restore_default_recording();
            audio_control::restore_endpoint_channels();
        }
    }
}

/// Packet-less stretch after which `DATA_DISCONTINUITY` is idle-resume, not a hole.
/// Classic loopback delivers nothing while nothing renders, then flags the resume packet;
/// scoring that flag always would charge every notification on a silent host. ~10 ms engine
/// period: 1 s is past anything this loop can still tell from a gap.
const LOOPBACK_IDLE_AFTER: Duration = Duration::from_secs(1);

fn capture_thread(
    tx: SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    ready: SyncSender<Result<()>>,
    channels: u32,
    rate_hz: u32,
    active: Arc<AtomicBool>,
    opened_rate: Arc<AtomicU32>,
) -> Result<()> {
    // COM is apartment-bound; MTA on this thread, before any device call.
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    // Must wake on the engine event every ~10 ms or the loopback buffer wraps. Same MMCSS +
    // `THREAD_PRIORITY_HIGHEST` boost as the paced sender; a no-op if refused.
    pf_frame::thread_qos::boost_thread_priority(true);
    let mut live = LiveCapture::new();
    // Each pass is one plan, open and loop. First open gets [`FIRST_OPEN_ATTEMPTS`] tries
    // before `open()` surfaces Err; later failures back off, or wait on the endpoint set for
    // [`PlanUnsatisfiable`].
    let mut ready = Some(ready);
    let mut mode = TargetMode::Assert;
    let mut failures: u64 = 0;
    let mut first_attempts: u32 = 0;
    let mut backoff = REOPEN_BACKOFF_START;
    // Plan is pure in the endpoint set: log the unsatisfiable diagnosis once per fingerprint.
    let mut unsat_logged: Option<u64> = None;
    while !stop.load(Ordering::Relaxed) {
        let attempt = Instant::now();
        let open = prepare_plan(mode);
        let pass =
            open_loopback(&open, channels, rate_hz, &opened_rate, &mut ready).and_then(|opened| {
                run_loop(
                    opened,
                    &open,
                    channels,
                    &tx,
                    &stop,
                    &active,
                    &mut live.voice,
                )
            });
        match pass {
            Ok(Next::Stopped) => break,
            Ok(Next::Reopen(m)) => {
                mode = m;
                failures = 0;
                backoff = REOPEN_BACKOFF_START;
                unsat_logged = None;
            }
            Err(e) if ready.is_some() => {
                // Unsatisfiable plan cannot improve inside the handshake — fail now rather than
                // spending the transient retry budget. The native plane owns first-open retries.
                if e.downcast_ref::<PlanUnsatisfiable>().is_some() {
                    let _ = ready.take().unwrap().send(Err(anyhow!("{e:#}")));
                    break;
                }
                first_attempts += 1;
                if first_attempts >= FIRST_OPEN_ATTEMPTS || stop.load(Ordering::Relaxed) {
                    let _ = ready.take().unwrap().send(Err(anyhow!("{e:#}")));
                    break;
                }
                tracing::info!(error = %format!("{e:#}"), attempt = first_attempts,
                    "audio loopback first open failed — retrying");
                // Stop-responsive; same 100 ms slices as the reopen wait below.
                let until = Instant::now() + FIRST_OPEN_RETRY_PAUSE;
                while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                }
            }
            Err(e) => {
                if let Some(unsat) = e.downcast_ref::<PlanUnsatisfiable>() {
                    // Same endpoints → same verdict. Wait on the fingerprint; a wiring retry
                    // would IPolicyConfig-stomp an operator recording-default change.
                    failures = 0;
                    backoff = REOPEN_BACKOFF_START;
                    if unsat_logged != Some(unsat.fingerprint) {
                        unsat_logged = Some(unsat.fingerprint);
                        tracing::error!(
                            "desktop audio unavailable, and retrying cannot help until the \
                             audio endpoint set changes — waiting for that change. {unsat}"
                        );
                    }
                    if wait_endpoint_change(&stop, unsat.fingerprint, None) == EndpointWait::Stopped
                    {
                        break;
                    }
                } else {
                    unsat_logged = None;
                    if attempt.elapsed() >= REOPEN_STABLE_AFTER {
                        failures = 0;
                        backoff = REOPEN_BACKOFF_START;
                    }
                    failures += 1;
                    // Follow keeps following the output the operator chose: one invalidated
                    // stream (a Bluetooth profile switch) must not take the default back. A
                    // second miss in a row re-plans.
                    if failures > 1 {
                        mode = TargetMode::Assert;
                    }
                    if failures.is_power_of_two() {
                        tracing::warn!(error = %format!("{e:#}"), count = failures,
                            backoff_secs = backoff.as_secs(),
                            "audio loopback capture failed — reopening after backoff");
                    }
                    // Cut short (and reset) when the set changes — a re-arrived device must not
                    // sit out the 60 s cap.
                    let fp = audio_control::endpoint_fingerprint();
                    match wait_endpoint_change(&stop, fp, Some(Instant::now() + backoff)) {
                        EndpointWait::Stopped => break,
                        EndpointWait::Changed => backoff = REOPEN_BACKOFF_START,
                        EndpointWait::Elapsed => backoff = (backoff * 2).min(REOPEN_BACKOFF_CAP),
                    }
                }
            }
        }
    }
    Ok(())
}

/// Wiring plan with no loopback endpoint. [`wiring_plan::plan`] is pure in the enumerated set,
/// so this is permanent until the topology changes — unlike every other capture error.
/// Carries the fingerprint the reopen loop waits on, plus the diagnosis.
#[derive(Debug)]
struct PlanUnsatisfiable {
    fingerprint: u64,
    detail: String,
}

impl PlanUnsatisfiable {
    fn from_plan(plan: &audio_control::WiredPlan) -> PlanUnsatisfiable {
        debug_assert!(plan.wiring.loopback_unsatisfiable());
        PlanUnsatisfiable {
            fingerprint: plan.fingerprint,
            detail: wiring_plan::describe_no_loopback(&plan.renders, &plan.wiring),
        }
    }
}

impl std::fmt::Display for PlanUnsatisfiable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for PlanUnsatisfiable {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EndpointWait {
    Stopped,
    /// Fingerprint moved — re-plan; this is the recovery.
    Changed,
    /// Deadline passed with no change. `deadline: None` never returns this.
    Elapsed,
}

/// Poll the endpoint-set fingerprint every [`ENDPOINT_POLL_EVERY`] (enumerate-and-hash only —
/// no wiring, no IPolicyConfig) until the set changes, `deadline` passes, or `stop` is set.
/// `deadline: None` waits indefinitely: used while the plan is unsatisfiable.
fn wait_endpoint_change(
    stop: &AtomicBool,
    fingerprint: u64,
    deadline: Option<Instant>,
) -> EndpointWait {
    let mut next_poll = Instant::now() + ENDPOINT_POLL_EVERY;
    loop {
        if stop.load(Ordering::Relaxed) {
            return EndpointWait::Stopped;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return EndpointWait::Elapsed;
        }
        thread::sleep(Duration::from_millis(100));
        if Instant::now() >= next_poll {
            next_poll = Instant::now() + ENDPOINT_POLL_EVERY;
            if audio_control::endpoint_fingerprint() != fingerprint {
                return EndpointWait::Changed;
            }
        }
    }
}

/// Silent on the host with a working loopback: the name rule, or the minted Speakers by id
/// ("Punktfunk Speakers" fails the name rule).
fn silent_loopback(name: &str, id: &str) -> bool {
    wiring_plan::silent_sink(&name.to_lowercase())
        || super::minted::minted_ids().speakers_render.as_deref() == Some(id)
}

/// Current default render endpoint (`None` on enumeration failure — a miss must not kill capture).
fn default_render(en: &DeviceEnumerator) -> Option<(Device, String)> {
    let d = en.get_default_device(&Direction::Render).ok()?;
    let id = d.get_id().ok()?;
    Some((d, id))
}

/// The endpoint to loopback-capture, as `(device, name, id)`: the plan's when `bind_plan`,
/// else the default render. Echo guard: the plan reserves `mic_render` for the virtual mic,
/// and capturing it streams the client's voice back to them, so a default that is the mic
/// falls back to the plan's loopback, or refuses.
fn choose_endpoint(
    plan: &audio_control::WiredPlan,
    bind_plan: bool,
    en: &DeviceEnumerator,
) -> Result<(Device, String, String)> {
    let wiring = &plan.wiring;
    if bind_plan {
        let Some(ep) = wiring.loopback_render.clone() else {
            // Typed: the plan is a pure function of the set, so wait on the fingerprint.
            return Err(PlanUnsatisfiable::from_plan(plan).into());
        };
        let d = audio_control::open_endpoint(&ep)?;
        return Ok((d, ep.0, ep.1));
    }
    let (default, id) =
        default_render(en).context("default render endpoint (loopback needs a render device)")?;
    let default_is_mic = wiring
        .mic_render
        .as_ref()
        .is_some_and(|(_, mic_id)| *mic_id == id);
    if !default_is_mic {
        let name = default.get_friendlyname().unwrap_or_default();
        return Ok((default, name, id));
    }
    let Some(lb) = wiring.loopback_render.clone() else {
        // Not [`PlanUnsatisfiable`]: Follow's inputs include the default, which the
        // operator can change without a topology change (esp. `PUNKTFUNK_KEEP_DEFAULT`).
        anyhow::bail!(
            "the default render endpoint is reserved for the virtual mic (capturing it \
             would echo the client's voice back) — {}",
            wiring_plan::describe_no_loopback(&plan.renders, wiring)
        );
    };
    tracing::warn!(mic = %wiring.mic_render.as_ref().unwrap().0, loopback = %lb.0,
        "default render endpoint is the virtual-mic target — loopback-capturing the plan's \
         endpoint instead");
    let d = audio_control::open_endpoint(&lb)?;
    Ok((d, lb.0, lb.1))
}

/// The rate to open at. The shared-mode mix format is authoritative: `AUTOCONVERTPCM`
/// succeeds on an upward request and returns interpolated samples, so never pad above the
/// engine. The floor is [`SAMPLE_RATE`], not the engine: libopus takes 8/12/16/24/48 kHz
/// only, so a 44.1 kHz endpoint still opens at 48 kHz. An unreadable mix format declines
/// hi-res, which can't cost a working 48 kHz session.
fn settle_open_rate(engine_hz: Option<u32>, requested: u32) -> u32 {
    match engine_hz {
        Some(hz) if hz > 0 && requested > hz.max(SAMPLE_RATE) => hz.max(SAMPLE_RATE),
        None => SAMPLE_RATE,
        _ => requested,
    }
}

/// Gap accounting for a polling loopback tap, in the device clock. The tap stops while the
/// endpoint idles, so "time since last data" would score every quiet as a hole; instead
/// `DATA_DISCONTINUITY` plus `index` (where the next packet starts if nothing was lost) sizes
/// the missing audio. Time is passed in.
struct LoopbackGaps {
    last_packet: Option<Instant>,
    /// The next packet's `index` if nothing is lost, in engine frames.
    next_index: u64,
    /// The engine rate `index` counts in; a packet's frames are at `open_hz`.
    index_hz: u64,
    open_hz: u64,
}

impl LoopbackGaps {
    fn new(engine_hz: Option<u32>, open_hz: u32) -> LoopbackGaps {
        LoopbackGaps {
            last_packet: None,
            next_index: 0,
            index_hz: u64::from(engine_hz.filter(|&hz| hz > 0).unwrap_or(open_hz).max(1)),
            open_hz: u64::from(open_hz.max(1)),
        }
    }

    /// A packet of `frames` (non-zero) starting at `index`. `Some` is the audio a discontinuity
    /// lost. A flag on the first packet after a quiet stretch is idle-resume, not a hole in
    /// anything that was playing; a SILENT packet's lost stretch was silence (an idling
    /// loopback delivers them on the 100 ms timeout, each flagged).
    fn packet(
        &mut self,
        now: Instant,
        index: u64,
        frames: u64,
        discontinuity: bool,
        silent: bool,
    ) -> Option<Duration> {
        let flowing = self
            .last_packet
            .is_some_and(|t| now.duration_since(t) < LOOPBACK_IDLE_AFTER);
        let lost = (discontinuity && flowing && !silent).then(|| {
            let lost = index.saturating_sub(self.next_index);
            Duration::from_micros(lost.saturating_mul(1_000_000) / self.index_hz)
        });
        self.next_index = index.saturating_add(frames * self.index_hz / self.open_hz);
        self.last_packet = Some(now);
        lost
    }
}

/// The default-render watchdog: an operator who picks another output mid-stream is followed.
/// Only a CHANGE of the default id reacts, so a permanently-denied default can't reopen-loop.
struct DefaultWatch {
    seen: Option<String>,
    fight: FightDamper,
    last_check: Instant,
}

impl DefaultWatch {
    /// Once per [`DEFAULT_CHECK_EVERY`]. `Some` = stop the stream and reopen.
    fn tick(
        &mut self,
        en: &DeviceEnumerator,
        wiring: &wiring_plan::Wiring,
        (dev_name, dev_id): (&str, &str),
        keep_default: bool,
        assert_plan: bool,
    ) -> Option<Next> {
        if self.last_check.elapsed() < DEFAULT_CHECK_EVERY {
            return None;
        }
        self.last_check = Instant::now();
        let (_, nid) = default_render(en)?;
        if self.seen.as_deref() == Some(nid.as_str()) {
            return None;
        }
        self.seen = Some(nid.clone());
        if nid == dev_id {
            return None;
        }
        if keep_default {
            tracing::info!("default render device changed (PUNKTFUNK_KEEP_DEFAULT) — following it");
            return Some(Next::Reopen(TargetMode::Follow));
        }
        match judge_default(wiring, &nid) {
            DefaultKind::Capturable(name) => {
                tracing::info!(device = %name,
                    "operator changed the output device mid-stream — following it (audio now \
                     also plays on the host)");
                Some(Next::Reopen(TargetMode::Follow))
            }
            // Follow/KEEP_DEFAULT capture IS the default — reopen on Assert.
            DefaultKind::Dud(_) if !assert_plan => Some(Next::Reopen(TargetMode::Assert)),
            // Assert binds capture to the plan's endpoint, not the default. Only where apps
            // render has moved — put the default back, keep the stream. A full reopen is a
            // dropout on every dud-default fight.
            DefaultKind::Dud(name) => {
                self.fight.observed_at(Instant::now());
                if self.fight.should_reassert() {
                    audio_control::reassert_default_playback(dev_id);
                    // Next watchdog tick sees our endpoint and stays quiet.
                    self.seen = Some(dev_id.to_string());
                    if self.fight.warn_now() {
                        tracing::warn!(device = %name, planned = %dev_name,
                            "something keeps moving the default playback to an endpoint whose \
                             loopback cannot work — putting it back (the capture is unaffected)");
                    }
                } else if self.fight.warn_giving_up() {
                    tracing::warn!(device = %name, planned = %dev_name,
                        backoff_s = FIGHT_BACKOFF.as_secs(),
                        "another program is repeatedly taking the default \
                         playback device — backing off rather than fighting it. \
                         Desktop audio keeps streaming from the planned endpoint, \
                         but apps rendering to the other device will not be heard");
                }
                None
            }
            DefaultKind::Unknown => Some(Next::Reopen(TargetMode::Assert)),
        }
    }
}

/// One open's wiring plan and the decisions read with it, so the open and its loop agree.
struct OpenPlan {
    wired: audio_control::WiredPlan,
    /// Capture the plan's loopback endpoint, not the default render ([`binding`]).
    bind_plan: bool,
    /// May park the playback default on that endpoint ([`binding`]).
    assert_plan: bool,
    keep_default: bool,
    seat: bool,
    /// Bound to the plan's last-resort pick, whose loopback is known-silent.
    last_resort: bool,
}

/// Plans this open. Client-only audio wants a silent sink with working loopback, so a plan
/// without one tries the Steam pair, once per INF state, and re-plans.
fn prepare_plan(mode: TargetMode) -> OpenPlan {
    let keep_default = audio_control::keep_default_devices();
    let seat = pf_paths::seat::is_seat_host();
    let (bind_plan, assert_plan) = binding(mode, keep_default, seat);
    let mut wired = audio_control::wire_now_full(assert_plan);
    if assert_plan && !audio_control::host_audio_requested() {
        // Without the minted-id half of [`silent_loopback`], a minted session re-attempts a
        // Steam-pair install it does not need.
        let have_silent = |w: &wiring_plan::Wiring| {
            w.loopback_render
                .as_ref()
                .is_some_and(|(n, id)| silent_loopback(n, id))
        };
        if !have_silent(&wired.wiring) && super::wasapi_mic::steam_pair_install_due() {
            if super::wasapi_mic::install_steam_audio_pair() {
                wired = audio_control::wire_now_full(true);
            }
            if !have_silent(&wired.wiring) {
                tracing::info!(
                    "no silent virtual sink for client-only audio — desktop audio will also play \
                     on the host (install Steam, whose Remote Play streaming drivers provide one)"
                );
            }
        }
    }
    // Last resort belongs to the plan's pick: a capture that follows the default never lands on
    // Steam Speakers, because `judge_default` calls them `excluded_from_loopback`.
    let last_resort = bind_plan && wired.wiring.loopback_last_resort;
    OpenPlan {
        wired,
        bind_plan,
        assert_plan,
        keep_default,
        seat,
        last_resort,
    }
}

/// A started loopback stream. Fields drop in order: capture client, event, audio client.
struct OpenedLoopback {
    capture: wasapi::AudioCaptureClient,
    event: wasapi::Handle,
    client: wasapi::AudioClient,
    en: DeviceEnumerator,
    dev_name: String,
    dev_id: String,
    engine_hz: Option<u32>,
    open_hz: u32,
}

/// Opens and starts the loopback stream on this plan's endpoint, then fires `ready`: `take`
/// empties it on the first open that starts, so it fires once and before any loop runs.
fn open_loopback(
    open: &OpenPlan,
    channels: u32,
    rate_hz: u32,
    opened_rate: &AtomicU32,
    ready: &mut Option<SyncSender<Result<()>>>,
) -> Result<OpenedLoopback> {
    let en = DeviceEnumerator::new().context("DeviceEnumerator")?;
    let (device, dev_name, dev_id) = choose_endpoint(&open.wired, open.bind_plan, &en)?;

    let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
    let mut engine = audio_client.get_mixformat().ok();
    // Apps render at the endpoint's channel count; autoconvert then only upmixes that. A silent
    // sink takes the session's layout until capture ends; any other endpoint is the operator's.
    if let Some(have) = engine
        .as_ref()
        .map(|f| f.get_nchannels())
        .filter(|&have| u32::from(have) != channels)
    {
        let shared = *live_captures() > 1;
        if silent_loopback(&dev_name, &dev_id) && shared {
            tracing::info!(device = %dev_name, engine_ch = have, requested = channels,
                "another session is capturing this sink — keeping its speaker layout");
        } else if silent_loopback(&dev_name, &dev_id) {
            let hz = engine
                .as_ref()
                .map_or(SAMPLE_RATE, |f| f.get_samplespersec());
            match audio_control::reshape_endpoint(&dev_id, have, channels as u16, hz) {
                Ok(()) => {
                    tracing::info!(device = %dev_name, from = have, to = channels,
                        "desktop-audio sink set to the session's speaker layout");
                    // Re-read so the open and its log see the new layout.
                    audio_client = device.get_iaudioclient().context("IAudioClient")?;
                    engine = audio_client.get_mixformat().ok();
                }
                Err(e) => tracing::warn!(device = %dev_name, error = %format!("{e:#}"),
                    engine_ch = have, requested = channels,
                    "desktop-audio sink kept its speaker layout — apps keep rendering that count"),
            }
        } else if u32::from(have) < channels {
            tracing::info!(device = %dev_name, engine_ch = have, requested = channels,
                "captured endpoint has fewer channels than the session — apps render at its \
                 count; set its speaker configuration in Windows' sound settings for surround");
        }
    }
    let engine_hz = engine.as_ref().map(|f| f.get_samplespersec());
    let open_hz = settle_open_rate(engine_hz, rate_hz);
    match engine_hz {
        _ if open_hz == rate_hz => {}
        Some(hz) => tracing::info!(
            device = %dev_name,
            engine_hz = hz,
            requested = rate_hz,
            opening_at = open_hz,
            "engine rate is below the requested capture rate — hi-res declined; opening at \
             the engine rate rather than letting WASAPI autoconvert upsample it (set this \
             endpoint's rate in Windows' device properties to raise it)"
        ),
        None => tracing::info!(
            device = %dev_name,
            requested = rate_hz,
            "endpoint mix format unreadable — hi-res declined; opening at the legacy rate"
        ),
    }
    // Before Initialize can fail — `sample_rate()` already has a decided answer.
    opened_rate.store(open_hz, Ordering::Relaxed);
    // Autoconvert matches the engine mix to this layout. `dwChannelMask` pins wire order
    // (FL FR FC LFE RL RR SL SR; 7.1 = 0x63F, not 0xFF). Loopback is implied by capturing a
    // RENDER device with `Direction::Capture` in shared mode.
    let mask = punktfunk_core::audio::wasapi_channel_mask(channels as u8);
    let desired = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        open_hz as usize,
        channels as usize,
        Some(mask),
    );
    // Do not pass `min_period`: shared-mode `Initialize` cannot change the engine period
    // (`hnsBufferDuration` sizes the buffer; the callback still fires at the default).
    // Lowering it needs `IAudioClient3::InitializeSharedAudioStream`, which `wasapi` does not wrap.
    let (default_period, min_period) = audio_client.get_device_period().context("device period")?;
    let stream_mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: default_period,
    };
    let used_period = default_period;
    audio_client
        .initialize_client(&desired, &Direction::Capture, &stream_mode)
        .context("initialize loopback client")?;
    let h_event = audio_client.set_get_eventhandle().context("event handle")?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .context("IAudioCaptureClient")?;
    audio_client
        .start_stream()
        .context("start loopback stream")?;
    if let Some(r) = ready.take() {
        let _ = r.send(Ok(()));
    }
    tracing::info!(device = %dev_name,
        follow = !open.bind_plan,
        last_resort = open.last_resort,
        // Asked vs settled — they differ only when an upward request was declined.
        requested_hz = rate_hz,
        opened_hz = open_hz,
        // Endpoint mix format, not the request.
        engine_hz = engine.as_ref().map(|f| f.get_samplespersec()),
        engine_ch = engine.as_ref().map(|f| f.get_nchannels()),
        engine_bits = engine.as_ref().map(|f| f.get_bitspersample()),
        buffer_ms = used_period as f32 / 10_000.0,
        min_buffer_ms = min_period as f32 / 10_000.0,
        "audio loopback capturing");
    if let Some(why) = &open.wired.wiring.loopback_narrowing {
        tracing::warn!(device = %dev_name,
            "capturing an endpoint that {why} — the stream cannot sound better than this source");
    }
    Ok(OpenedLoopback {
        capture: capture_client,
        event: h_event,
        client: audio_client,
        en,
        dev_name,
        dev_id,
        engine_hz,
        open_hz,
    })
}

/// The operator's own output, while this capture owns the defaults: voice apps get pinned to
/// it, and `host_and_client` renders the mix to it (the plan kept the silent sink). The render
/// opens only when the capture is silent on the host: a plan that fell back to real hardware
/// is already audible, and a second render would play the mix twice.
fn host_output(
    open: &OpenPlan,
    opened: &OpenedLoopback,
    channels: u32,
    voice: &mut voice_route::VoiceRoute,
) -> Option<Playthrough> {
    let host_out = (open.bind_plan && !open.keep_default)
        .then(audio_control::parked_previous_render)
        .flatten()
        .filter(|id| *id != opened.dev_id);
    if let Some(id) = &host_out {
        voice.arm(id);
    } else if voice_route::wanted() {
        static NOTED: std::sync::Once = std::sync::Once::new();
        NOTED.call_once(|| {
            tracing::warn!(
                "voice chat on the host is set, but this capture parked no host output to pin \
                 voice apps to (PUNKTFUNK_KEEP_DEFAULT, a seat, or the default already was the \
                 sink) — voice apps stay on the default and reach the stream"
            )
        });
    }
    match &host_out {
        Some(id) if audio_control::playthrough_requested() => {
            if silent_loopback(&opened.dev_name, &opened.dev_id) {
                Playthrough::open(id, channels, opened.open_hz)
            } else {
                tracing::info!(device = %opened.dev_name,
                    "host playthrough not needed — the captured endpoint is audible on the host");
                None
            }
        }
        _ => None,
    }
}

/// Assert's park did not stick when `default`, read right after open, is another endpoint.
/// `Some` follows a capturable default; a dud warns and keeps the planned endpoint.
fn converge_default(
    wiring: &wiring_plan::Wiring,
    (dev_name, dev_id): (&str, &str),
    default: Option<&str>,
) -> Option<Next> {
    let d = default.filter(|d| *d != dev_id)?;
    match judge_default(wiring, d) {
        DefaultKind::Capturable(name) => {
            tracing::info!(default = %name, planned = %dev_name,
                "could not park the default playback on the planned endpoint — \
                 capturing the actual default instead (audio audible on the host)");
            Some(Next::Reopen(TargetMode::Follow))
        }
        DefaultKind::Dud(name) => {
            tracing::warn!(default = %name, planned = %dev_name,
                "default playback stayed on an endpoint whose loopback cannot work — \
                 capturing the planned endpoint; desktop audio may be silent");
            None
        }
        DefaultKind::Unknown => None,
    }
}

/// Reads every ready packet into `bytes`, zeroing SILENT ones, and scores holes and empty
/// reads. `true` when any packet was ready.
fn read_packets(
    capture: &wasapi::AudioCaptureClient,
    bytes: &mut VecDeque<u8>,
    block_align: usize,
    gaps: &mut LoopbackGaps,
    stats: &mut CaptureStats,
) -> Result<bool> {
    let mut saw = false;
    loop {
        match capture.get_next_packet_size() {
            Ok(Some(0)) | Ok(None) => return Ok(saw),
            Ok(Some(_n)) => {
                saw = true;
                let before = bytes.len();
                let info = capture
                    .read_from_device_to_deque(bytes)
                    .context("read loopback")?;
                // WASAPI: a SILENT packet's data is not defined; it is silence.
                if info.flags.silent {
                    bytes.range_mut(before..).for_each(|b| *b = 0);
                }
                let frames = ((bytes.len() - before) / block_align) as u64;
                if frames == 0 {
                    // Packet-ready then zero frames: a spinning tap looks like a quiet desktop.
                    stats.missed_dequeues += 1;
                } else if let Some(lost) = gaps.packet(
                    Instant::now(),
                    info.index,
                    frames,
                    info.flags.data_discontinuity,
                    info.flags.silent,
                ) {
                    stats.observe_gap(lost);
                }
            }
            Err(e) => return Err(anyhow!("get_next_packet_size: {e}")),
        }
    }
}

/// Streams `opened` until stop, a default change [`DefaultWatch`] follows, or a last-resort
/// plan's endpoint set moving. The default seen right after open seeds the watch: only a
/// later CHANGE of that id reacts, so a permanently-denied default set cannot reopen-loop.
fn run_loop(
    opened: OpenedLoopback,
    open: &OpenPlan,
    channels: u32,
    tx: &SyncSender<Vec<f32>>,
    stop: &AtomicBool,
    active: &AtomicBool,
    voice: &mut voice_route::VoiceRoute,
) -> Result<Next> {
    let wiring = &open.wired.wiring;
    let ids = (opened.dev_name.as_str(), opened.dev_id.as_str());
    let mut playthrough = host_output(open, &opened, channels, voice);
    let seen_default = default_render(&opened.en).map(|(_, id)| id);
    if open.assert_plan {
        if let Some(next) = converge_default(wiring, ids, seen_default.as_deref()) {
            return Ok(next);
        }
    }

    // 4 bytes per f32 sample, interleaved.
    let block_align = channels as usize * 4;
    let mut bytes: VecDeque<u8> = VecDeque::new();
    let mut last_fp_check = Instant::now();
    // 30 s with zero packets: a broken loopback looks like a quiet desktop. Info, not warn —
    // idle hosts are silent — except last-resort, where the plan already knew the tap is silent.
    let opened_at = Instant::now();
    let mut saw_packets = false;
    let mut silence_noted = false;
    // Periodic vitals. A stalled encode drops chunks with no other log line; the encoder
    // concatenates across the hole (click + permanent A/V offset).
    let mut stats = CaptureStats::default();
    let mut last_stats = Instant::now();
    let mut gaps = LoopbackGaps::new(opened.engine_hz, opened.open_hz);
    let mut watch = DefaultWatch {
        seen: seen_default,
        fight: FightDamper::new(Instant::now()),
        last_check: Instant::now(),
    };
    loop {
        if stop.load(Ordering::Relaxed) {
            opened.client.stop_stream().ok();
            return Ok(Next::Stopped);
        }
        // Events fire only while audio renders; finite timeout keeps `stop` and the watchdog alive.
        let _ = opened.event.wait_for_event(100);
        voice.tick();
        saw_packets |= read_packets(
            &opened.capture,
            &mut bytes,
            block_align,
            &mut gaps,
            &mut stats,
        )?;
        if !saw_packets && !silence_noted && opened_at.elapsed() >= Duration::from_secs(30) {
            silence_noted = true;
            if open.last_resort {
                tracing::warn!(device = %opened.dev_name,
                    "no audio captured in the first 30 s from the LAST-RESORT loopback — the \
                     Steam Streaming Speakers' loopback is known-silent, so desktop audio is \
                     most likely not reaching the client; attach any output device to give the \
                     plan a working endpoint (it re-plans on the change)");
            } else {
                tracing::info!(device = %opened.dev_name,
                    "no audio captured in the first 30 s — fine if the host is quiet; if it \
                     should be playing audio, this endpoint's loopback may be broken (set \
                     PUNKTFUNK_HOST_AUDIO=1 to prefer real hardware)");
            }
        }
        let whole = (bytes.len() / block_align) * block_align;
        if whole > 0 {
            let raw: Vec<u8> = bytes.drain(..whole).collect();
            let mut samples = Vec::with_capacity(whole / 4);
            for c in raw.chunks_exact(4) {
                samples.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
            stats.observe(&samples, channels);
            if let Some(p) = playthrough.as_mut() {
                p.write(&samples);
            }
            // Lossy, non-blocking. Count only while a session is reading: a full channel under
            // a live consumer is encode lag (click + permanent shift). A parked capturer fills
            // once and then refuses everything ([`WasapiLoopbackCapturer::active`]).
            if tx.try_send(samples).is_err() && active.load(Ordering::Relaxed) {
                stats.dropped_chunks += 1;
            }
        }
        // Gaps here come from WASAPI discontinuity, not callback cadence: an idle-then-resume
        // endpoint is not a gap ([`LOOPBACK_IDLE_AFTER`]).
        stats.flush_window(&mut last_stats, opened.open_hz, Some(ids.0));

        // A seat has no operator default: the box's is somebody else's, and following it
        // streams their audio.
        if !open.seat {
            if let Some(next) =
                watch.tick(&opened.en, wiring, ids, open.keep_default, open.assert_plan)
            {
                opened.client.stop_stream().ok();
                return Ok(next);
            }
        }

        // Last-resort is a stopgap: any endpoint-set change may unlock a real plan. Preferred
        // endpoints don't watch this — mid-stream re-routing is the default-device watchdog.
        if open.last_resort && last_fp_check.elapsed() >= ENDPOINT_POLL_EVERY {
            last_fp_check = Instant::now();
            if audio_control::endpoint_fingerprint() != open.wired.fingerprint {
                opened.client.stop_stream().ok();
                tracing::info!(
                    "endpoint set changed while capturing the last-resort loopback — re-planning"
                );
                return Ok(Next::Reopen(TargetMode::Assert));
            }
        }
    }
}

/// `host_and_client` with voice chat on the host: the capture stays on the silent sink and
/// this render stream on the operator's output is how the host hears the mix. Polled from
/// the capture loop; a full buffer drops the excess rather than stalling the capture.
struct Playthrough {
    client: wasapi::AudioClient,
    render: wasapi::AudioRenderClient,
    channels: usize,
    dropped_frames: u64,
}

impl Playthrough {
    /// `None` on any failure, logged once: the stream keeps going without the host hearing it.
    fn open(device_id: &str, channels: u32, rate_hz: u32) -> Option<Playthrough> {
        match Self::try_open(device_id, channels, rate_hz) {
            Ok((p, name)) => {
                tracing::info!(device = %name, "host playthrough: rendering the stream mix to the operator's output");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"),
                    "host playthrough not opened — the host will not hear the stream mix");
                None
            }
        }
    }

    fn try_open(device_id: &str, channels: u32, rate_hz: u32) -> Result<(Playthrough, String)> {
        let device = super::pad_endpoint::open_wasapi_device(device_id)?;
        let name = device.get_friendlyname().unwrap_or_default();
        let mut client = device.get_iaudioclient().context("IAudioClient")?;
        // Same layout the capture delivers; autoconvert matches the device's engine format.
        let mask = punktfunk_core::audio::wasapi_channel_mask(channels as u8);
        let desired = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            rate_hz as usize,
            channels as usize,
            Some(mask),
        );
        let (default_period, _) = client.get_device_period().context("device period")?;
        // Three engine periods (~30 ms): room for one late capture wake without a hole.
        let mode = StreamMode::PollingShared {
            autoconvert: true,
            buffer_duration_hns: default_period * 3,
        };
        client
            .initialize_client(&desired, &Direction::Render, &mode)
            .context("initialize playthrough render client")?;
        let render = client
            .get_audiorenderclient()
            .context("IAudioRenderClient")?;
        let frames = client.get_buffer_size().context("buffer size")? as usize;
        let _ = render.write_to_device(frames, &vec![0u8; frames * channels as usize * 4], None);
        client.start_stream().context("start playthrough stream")?;
        Ok((
            Playthrough {
                client,
                render,
                channels: channels as usize,
                dropped_frames: 0,
            },
            name,
        ))
    }

    fn write(&mut self, samples: &[f32]) {
        let frames = samples.len() / self.channels;
        let space = self.client.get_available_space_in_frames().unwrap_or(0) as usize;
        let n = frames.min(space);
        self.dropped_frames += (frames - n) as u64;
        if n == 0 {
            return;
        }
        let bytes: Vec<u8> = samples[..n * self.channels]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        if let Err(e) = self.render.write_to_device(n, &bytes, None) {
            tracing::debug!(error = %e, "playthrough write");
        }
    }
}

impl Drop for Playthrough {
    fn drop(&mut self) {
        let _ = self.client.stop_stream();
        if self.dropped_frames > 0 {
            tracing::debug!(dropped_frames = self.dropped_frames, "playthrough ended");
        }
    }
}

/// Watchdog verdict on a newly-observed default render endpoint.
enum DefaultKind {
    /// Following it yields working audio (audible on the host too).
    Capturable(String),
    /// Mic target, pad endpoint, or known-silent/echoing loopback. Following it is silence or echo.
    Dud(String),
    /// Enumeration miss (transient churn).
    Unknown,
}

/// Resolve via [`super::pad_endpoint::open_wasapi_device`], not `DeviceEnumerator::get_device`:
/// that handed `GetDevice` a freed string through 0.23, and a miss here silently downgrades a
/// capturable default to `Unknown`. Keep one resolution path — see the helper.
fn judge_default(wiring: &wiring_plan::Wiring, id: &str) -> DefaultKind {
    let Ok(dev) = super::pad_endpoint::open_wasapi_device(id) else {
        return DefaultKind::Unknown;
    };
    let name = dev.get_friendlyname().unwrap_or_default();
    let ln = name.to_lowercase();
    let is_mic = wiring
        .mic_render
        .as_ref()
        .is_some_and(|(_, mic_id)| mic_id == id);
    // Pad endpoints are stamped with the controller name so games treat them as the pad speaker;
    // `excluded_from_loopback` therefore passes them as Capturable. This classifier also drives
    // the watchdog and Follow, so a pad default would send the desktop mix to voice coils.
    // Identity, not name.
    let is_pad = super::pad_endpoint::is_pad_render_endpoint(id);
    if is_mic || is_pad || wiring_plan::excluded_from_loopback(&ln) {
        DefaultKind::Dud(name)
    } else {
        DefaultKind::Capturable(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seat rule may only add plan binding. Every console verdict is what it always was.
    #[test]
    fn a_seat_binds_the_plan_and_still_parks_nothing() {
        for keep in [false, true] {
            for mode in [TargetMode::Assert, TargetMode::Follow] {
                let was = mode == TargetMode::Assert && !keep;
                assert_eq!(
                    binding(mode, keep, false),
                    (was, was),
                    "console keep={keep}"
                );
            }
        }
        // A seat is `keep_default` by construction (`audio_control::keep_default_devices`).
        assert_eq!(binding(TargetMode::Assert, true, true), (true, false));
    }

    /// Never above the engine, never below what libopus takes, and legacy when unreadable.
    #[test]
    fn the_open_rate_never_upsamples_past_the_engine() {
        assert_eq!(settle_open_rate(Some(48_000), 96_000), 48_000);
        assert_eq!(settle_open_rate(Some(44_100), 96_000), SAMPLE_RATE);
        assert_eq!(settle_open_rate(Some(192_000), 96_000), 96_000);
        assert_eq!(settle_open_rate(Some(44_100), 44_100), 44_100);
        assert_eq!(settle_open_rate(Some(0), 96_000), 96_000);
        assert_eq!(settle_open_rate(None, 96_000), SAMPLE_RATE);
        assert_eq!(settle_open_rate(None, SAMPLE_RATE), SAMPLE_RATE);
    }

    /// A discontinuity mid-flow is sized in the device clock; one after an idle stretch, or on
    /// a silent packet, is not a hole.
    #[test]
    fn loopback_gaps_are_sized_from_the_device_index() {
        let t = Instant::now();
        let ms = Duration::from_millis;
        let mut g = LoopbackGaps::new(Some(48_000), 48_000);
        assert_eq!(
            g.packet(t, 0, 480, true, false),
            None,
            "nothing was flowing"
        );
        // The next packet should start at 480 and starts at 960: 480 frames, 10 ms, lost.
        assert_eq!(g.packet(t + ms(20), 960, 480, true, false), Some(ms(10)));
        assert_eq!(
            g.packet(t + ms(30), 2_400, 480, true, true),
            None,
            "a silent packet"
        );
        assert_eq!(
            g.packet(t + ms(2_000), 9_000, 480, true, false),
            None,
            "idle resume"
        );
        // A 44.1 kHz engine under a 48 kHz open: the index advances in engine frames.
        let mut g = LoopbackGaps::new(Some(44_100), 48_000);
        assert_eq!(g.packet(t, 0, 480, false, false), None);
        assert_eq!(g.next_index, 441);
    }

    /// Live loopback round trip. Skipped unless `PUNKTFUNK_WASAPI_LIVE=1` and a render endpoint exists.
    #[test]
    fn live_open_and_read() {
        if std::env::var("PUNKTFUNK_WASAPI_LIVE").is_err() {
            return;
        }
        let mut cap = match WasapiLoopbackCapturer::open(2, SAMPLE_RATE) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("no render endpoint on this box ({e:#}) — skipping");
                return;
            }
        };
        assert_eq!(cap.channels(), 2);
        // Legacy rate is never an upward request, so the settled rate must equal the ask.
        assert_eq!(cap.sample_rate(), SAMPLE_RATE);
        match cap.next_chunk() {
            Ok(samples) => assert!(
                samples.len() % 2 == 0,
                "interleaved stereo => even sample count"
            ),
            Err(e) => eprintln!("no audio within timeout (silent system?): {e:#}"),
        }
    }
}
