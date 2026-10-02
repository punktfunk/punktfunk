//! Android audio playback: decode the negotiated Opus (`0xC9`) or lossless PCM (`0xD3`)
//! plane to interleaved f32 and feed AAudio through a jitter ring.
//!
//! [`PlaneFormat`] fixes rate, depth, frame size, and the host-resolved stereo/5.1/7.1
//! layout for the session. [`open_ladder`] tries viable AAudio modes at that exact rate,
//! [`arm`] verifies that callbacks start, and [`supervise`] reopens disconnected devices.
//! Unsupported rates are rejected before `Hello` by [`output_rate_is_openable`]; playback
//! never silently resamples negotiated audio.
//!
//! The realtime callback recycles buffers rather than allocating. The shared
//! `JitterTuning::AAUDIO` policy handles drift and underruns, while `DisplayTracker` supplies
//! the video reference for A/V sync. See `design/hi-res-audio.md` and
//! `design/audio-latency-overhaul.md` for the full policy.

use crate::sys::sysprop;
use ndk::audio::{
    AudioCallbackResult, AudioContentType, AudioDirection, AudioFormat, AudioPerformanceMode,
    AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage,
};
use punktfunk_core::audio::plane::{PlaneDecoder, PlaneFormat};
use punktfunk_core::client::{AudioPacket, NativeClient};
use punktfunk_core::error::PunktfunkError;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

/// What one playback open attempt yields: the stream, plus both halves of the PCM hand-off — the
/// sender the decode thread fills and the receiver that returns drained buffers for refill.
///
/// A named struct rather than the tuple this used to be, because the tuple's type tripped
/// `clippy::type_complexity` (the Android target is linted since `:kit:cargoNdkClippy`) and because
/// the open path now carries the rung it succeeded on into the logs.
struct LiveStream {
    stream: AudioStream, // dropping it closes the AAudio stream
    tx: SyncSender<Vec<f32>>,
    free_rx: Receiver<Vec<f32>>,
    rung: OpenRung,
}

/// One rung of the AAudio open ladder — a sharing mode, a performance mode and the sample rate
/// they are tried at.
///
/// `rate` is `Some(hz)` for an explicit request (AAudio's contract: an explicitly-set rate is
/// honoured or the open FAILS — it never silently substitutes) and `None` for AAUDIO_UNSPECIFIED,
/// which lets the HAL name its own. The unspecified rung is not a licence to play at whatever came
/// back: [`arm`] accepts it only when the granted rate equals the session's, so it rescues the
/// device that refuses an explicit 48 000/96 000 while already running at it, and rejects the one
/// that would have handed us a different rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenRung {
    sharing: AudioSharingMode,
    perf: AudioPerformanceMode,
    rate: Option<i32>,
}

/// Why [`decode_loop`] returned — only one of them is worth reopening the device for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodeExit {
    /// `AudioPlayback` was dropped (session teardown, or Kotlin's `nativeStopAudio`).
    Shutdown,
    /// AAudio reported the stream disconnected. The stream is now dead by AAudio's contract and
    /// the only recovery is close + open a new one — see [`supervise`].
    Disconnected,
    /// The connector closed: no more audio is coming, so there is nothing to reopen FOR.
    SessionClosed,
    /// The plane cannot run at all: the Opus decoder would not build, because libopus refuses the
    /// negotiated rate or the host named a coupling this build does not know. Reopening the
    /// DEVICE would not change that, so it is not a reason to walk the ladder again.
    Fatal,
}

/// Decoded-chunk hand-off depth: 64 frames of slack (matches the core's AUDIO_QUEUE).
const RING_CHUNKS: usize = 64;

/// How long [`arm`] waits for a freshly started stream's FIRST data callback before writing the
/// rung off. Generous: a LowLatency stream calls back every few ms, and even a legacy path with a
/// large HDMI period is well inside this. The ring is un-primed for all of it, so what the device
/// pulls here is the priming silence — the point is only to prove that it pulls at all.
const START_WATCHDOG_MS: u64 = 400;
const START_WATCHDOG_POLL_MS: u64 = 10;
/// Settling time between a disconnect and the reopen. An HDMI route change (a TV switching mode,
/// an AVR re-handshaking) is not instantaneous, and reopening into the middle of one just spends
/// the ladder on rungs that were always going to fail.
const REOPEN_SETTLE_MS: u64 = 250;
/// How many times a reopen may find no usable device before the plane gives up — ~2 s of trying,
/// which comfortably outlasts an HDMI mode switch. Bounded rather than infinite so a device that
/// disconnects permanently (unplugged, claimed by another app for good) settles into silence
/// instead of a forever loop of opens on the session's audio thread.
const REOPEN_ATTEMPTS: u32 = 8;
/// Packets decoded with AAudio never having taken a single sample before we call it — expressed
/// as a DURATION, because the two planes do not agree on what a packet is worth: 5 ms on Opus, and
/// down to the ladder's shortest 1 ms rung on a `0xD3` session that is 24-bit surround or at the
/// top of the rate ladder. A packet count would have meant ~0.2 s there and a warning that fires
/// before a slow HAL has finished waking.
const DEAD_STREAM_WARN_MS: u64 = 1_000;

// --- Jitter-ring depths now come from the SHARED policy (`punktfunk_core::audio::JitterTuning`). --
// They used to be four Android-only constants here. The rationale for Android being DEEPER than the
// other clients still holds and is preserved in `JitterTuning::AAUDIO`: unlike PipeWire, which
// adaptively rate-matches the stream to the graph clock and masks host↔DAC drift, AAudio hands us a
// raw callback and we own the buffer, so drift and Wi-Fi power-save bunching land as
// underruns/overflows = crackle.
//
// Two things changed with the move. The prime floor drops 40 ms → 25 ms, because the policy GROWS
// the target on the devices that actually underrun instead of every device pre-paying for the worst
// one. And the ring finally sheds: it had a hard cap but nothing that walked the depth back down, so
// any drift or burst raised latency permanently and Android converged on its 120 ms ceiling and
// stayed there — the "audio latency is too high" report.
/// Throttle the AAudio XRun-driven HW-buffer grow check (cheap, but no need to poll every quantum).
const XRUN_CHECK_EVERY: u32 = 128;

/// Diagnostics — written by the decode thread + the realtime callback, logged periodically. The
/// audio analogue of the video `fed`/`rendered` counters (we can't "screenshot" sound).
///
/// The ring's DEPTH is not here: the A/V sync loop needs the same number in the same units, so it
/// is published once through [`punktfunk_core::audio::AudioSyncCell`] and read from there by the
/// log line below. One publisher, one reading — a second copy is a second thing to go stale.
#[derive(Default)]
struct Counters {
    // Wire frames decoded OK — Opus packets off `0xC9` (~200/s at 5 ms) or PCM frames off `0xD3`
    // (up to 1 000/s at the ladder's 1 ms rung, which 24-bit surround and the top of the rate
    // ladder land on). One counter for both planes because only one of them ever runs.
    frames_decoded: AtomicU64,
    pcm_written: AtomicU64, // PCM frames copied out to AAudio (device clock is pulling)
    underruns: AtomicU64,   // callbacks that emitted silence (ring not primed / drained)
    target_ms: AtomicU64,   // the policy's LIVE target depth (it grows on this device's underruns)
    /// Sync-driven inserts: one duplicated, crossfaded frame each (`JitterStep::insert_front`).
    /// Concealment must be visible next to the underruns it prevents — a ring that is quietly
    /// being deepened is a link whose picture keeps moving away from its audio.
    inserts: AtomicU64,
    /// Data callbacks since the process started, primed or not. Distinct from `pcm_written`
    /// (which only counts SERVED reads) because that is exactly the distinction the start
    /// watchdog needs: a device that is pulling but un-primed still ticks this, a stream that
    /// opened into the void ticks nothing at all. See [`wait_for_first_callback`].
    callbacks: AtomicU64,
}

/// Whether the A/V sync loop runs this session. `false` leaves `JitterPolicy`'s sync target at
/// `None`, which reproduces the pre-overhaul ring behaviour exactly — the point of the hatch.
///
/// Two levers because Android has neither of the other clients' launch surfaces. `PUNKTFUNK_NO_AV_SYNC`
/// keeps the contract the desktop clients document (and works when the client is driven from a
/// shell), but an app started from the launcher inherits no such environment, so the one a field
/// tester can actually reach is the sysprop — `adb shell setprop debug.punktfunk.no_av_sync 1`,
/// no rebuild, exactly like `debug.punktfunk.presenter`. A loop that steers PLAYBACK has to be
/// bisectable on the device that reports the regression, not only on the bench.
fn av_sync_enabled() -> bool {
    if matches!(
        std::env::var("PUNKTFUNK_NO_AV_SYNC").as_deref(),
        Ok("1") | Ok("true")
    ) {
        return false;
    }
    !matches!(
        sysprop(c"debug.punktfunk.no_av_sync").as_deref(),
        Some("1") | Some("true")
    )
}

/// Is this an Android TV / set-top box (as opposed to a phone, tablet or handheld)?
///
/// `ro.build.characteristics` carries a comma-separated trait list and contains `tv` on every
/// Android TV build. Read natively rather than plumbed down from Kotlin's `FEATURE_LEANBACK`
/// (which `StreamScreen` already computes for the video plane) to keep this decision inside the
/// audio module — the JNI entry point's signature is a compatibility surface for the kit, and the
/// only thing that wants this fact is [`open_ladder`]. An unset property reads as "not a TV",
/// which keeps the pre-existing behaviour on anything that does not answer.
fn is_tv_device() -> bool {
    sysprop(c"ro.build.characteristics")
        .is_some_and(|s| s.split(',').any(|trait_| trait_.trim() == "tv"))
}

/// Owned by [`crate::session::SessionHandle`]: the supervisor thread that owns the AAudio stream
/// and the decode loop for as long as the session lives.
pub struct AudioPlayback {
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AudioPlayback {
    /// Spawn the audio supervisor: it opens AAudio at the host-RESOLVED format by walking
    /// [`open_ladder`], runs the decode loop against it, and reopens it if the device disconnects.
    /// `None` only if the thread itself could not be spawned — an open failure is reported by the
    /// supervisor (the caller leaves video streaming either way).
    ///
    /// `game_audio` (the experimental low-latency mode) tags the stream usage=Game for the HAL's
    /// game-audio routing; off, the stream is untagged as it was before the overhaul. `is_tv` is
    /// Kotlin's `FEATURE_LEANBACK` and steers the ladder — see [`open_ladder`].
    pub fn start(
        client: Arc<NativeClient>,
        game_audio: bool,
        is_tv: bool,
    ) -> Option<AudioPlayback> {
        // Everything about the format comes from what the host RESOLVED, never from what this
        // device asked for: the channel count (2 = stereo / 6 = 5.1 / 8 = 7.1, canonical wire
        // order FL FR FC LFE RL RR SL SR), the plane, the rate, the depth and the frame duration.
        let fmt = PlaneFormat::of(&client);
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let join = std::thread::Builder::new()
            .name("pf-audio".into())
            .spawn(move || supervise(client, game_audio, is_tv, fmt, &sd))
            .ok()?;
        Some(AudioPlayback {
            shutdown,
            join: Some(join),
        })
    }
}

/// Check before `Hello` whether this device plays the requested rate without resampling.
///
/// Audio cannot be renegotiated mid-session, so the caller uses `false` to omit hi-res or choose
/// a lower wire rate. An explicit rate is not proof: the legacy path grants almost any rate and
/// AudioFlinger resamples it to the mixer, off the fast track, while the stream still reports the
/// rate asked for. The rate an unspecified low-latency open picks is the output's own; only that
/// one plays as sent.
///
/// Never starts the stream, and drops it immediately. `channels` is the requested layout because
/// the host-resolved layout does not exist until `Welcome`.
pub fn output_rate_is_openable(rate_hz: u32, channels: u8) -> bool {
    let built = AudioStreamBuilder::new().map(|b| {
        b.direction(AudioDirection::Output)
            .channel_count(punktfunk_core::audio::normalize_channels(channels) as i32)
            // The same f32 device format playback uses — see `try_open`. The wire depth is a wire
            // fact and never reaches AAudio, so probing at 24-bit would be probing the wrong thing.
            .format(AudioFormat::PCM_Float)
            .sharing_mode(AudioSharingMode::Shared)
            .performance_mode(AudioPerformanceMode::LowLatency)
            .open_stream()
    });
    match built {
        Ok(Ok(stream)) => {
            let native = stream.sample_rate();
            if native != rate_hz as i32 {
                log::info!(
                    "audio: this output runs at {native} Hz, so {rate_hz} Hz would be resampled — not offered"
                );
                return false;
            }
            true
        }
        Ok(Err(e)) => {
            log::info!("audio: this device will not open an output to probe its rate ({e})");
            false
        }
        Err(e) => {
            // No builder at all is a broken AAudio, not a verdict about the rate. Say no: the
            // caller's fallback is the legacy 48 kHz plane, which is the safe answer either way.
            log::warn!("audio: AAudio stream builder unavailable for the {rate_hz} Hz probe ({e})");
            false
        }
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        // The supervisor stopped + closed the AAudio stream on its way out.
    }
}

/// AAudio configurations to try, best first.
///
/// Phones prefer Exclusive/LowLatency; TVs start at Shared because some HDMI HALs open MMAP
/// streams that never route. Every mode is tried at the negotiated rate before one final
/// unspecified-rate attempt, which [`arm`] accepts only if AAudio chose that same rate. No rung
/// uses a different explicit rate.
///
/// `debug.punktfunk.audio_sharing` and `debug.punktfunk.audio_perf` can pin mode choices for
/// field diagnosis. Rate cannot be overridden because it must match the wire format.
fn open_ladder(is_tv: bool, fmt: PlaneFormat) -> Vec<OpenRung> {
    use AudioPerformanceMode::{LowLatency, None as PerfNone};
    use AudioSharingMode::{Exclusive, Shared};
    let mut modes: Vec<(AudioSharingMode, AudioPerformanceMode)> =
        match sysprop(c"debug.punktfunk.audio_sharing").as_deref() {
            Some("exclusive") => vec![(Exclusive, LowLatency), (Exclusive, PerfNone)],
            Some("shared") => vec![(Shared, LowLatency), (Shared, PerfNone)],
            _ if is_tv => vec![(Shared, LowLatency), (Shared, PerfNone)],
            _ => vec![
                (Exclusive, LowLatency),
                (Shared, LowLatency),
                (Shared, PerfNone),
            ],
        };
    // Not every device honours LowLatency (it is a request, like everything else on the builder),
    // and a HAL that mishandles it is exactly the sort we are laddering around — so `none` has to
    // be reachable as a forced choice, not only as the last rung.
    match sysprop(c"debug.punktfunk.audio_perf").as_deref() {
        Some("none") => modes.iter_mut().for_each(|m| m.1 = PerfNone),
        Some("lowlatency") | Some("low") => modes.iter_mut().for_each(|m| m.1 = LowLatency),
        _ => {}
    }
    modes.dedup();
    let mut rungs = Vec::with_capacity(modes.len() * 2);
    for rate in [Some(fmt.rate_hz as i32), None] {
        for &(sharing, perf) in &modes {
            rungs.push(OpenRung {
                sharing,
                perf,
                rate,
            });
        }
    }
    rungs
}

/// Everything an open attempt needs that does not vary between rungs.
struct OpenCtx<'a> {
    /// The session's resolved format — what the ring is sized in and what [`arm`] holds an
    /// unspecified-rate rung to.
    fmt: PlaneFormat,
    tuning: punktfunk_core::audio::JitterTuning,
    hard_cap_max: usize,
    game_audio: bool,
    counters: &'a Arc<Counters>,
    sync: &'a Arc<punktfunk_core::audio::AudioSyncCell>,
    /// Set by the AAudio error callback when the device disconnects — see [`supervise`].
    disconnected: &'a Arc<AtomicBool>,
    /// The session is closing: the ladder stops between rungs, so a stop waits for one open.
    shutdown: &'a AtomicBool,
}

/// Why a rung that opened could not be used.
enum ArmError {
    /// The grant did not match the request, or the start itself failed. This rung is out.
    Unusable(String),
    /// It opened and started but produced no data callback in time. Probably a device that cannot
    /// route this configuration — but possibly just a slow one, which is why this is the one
    /// failure [`open_any`] is willing to come back to.
    NotPulling,
}

/// Log the configuration a stream actually came up with.
///
/// The GRANTED modes, which need not be the ones asked for: AAudio may resolve an Exclusive
/// request to Shared and LowLatency to None, and `perf != LowLatency` means it fell to a legacy
/// path with different burst behaviour. Printing both sides is what lets a field log distinguish
/// that from plain jitter.
///
/// The RATE used to be diagnostic here too — a granted rate other than 48 000 was the tell for
/// that legacy path. It is now load-bearing instead: [`arm`] refuses any rung whose granted rate
/// is not the one the session negotiated, because playing a 96 kHz wire through a 48 kHz stream is
/// not a tuning problem, it is the wrong audio (§9). So this line can only ever print the rate the
/// session resolved — which is exactly why it still prints it: it is the field-log proof that the
/// device really opened at the rate the `Welcome` claimed.
fn log_started(live: &LiveStream, proven: bool) {
    let s = &live.stream;
    log::info!(
        "audio: AAudio started rate={} ch={} fmt={:?} perf={:?} share={:?} burst={} buf={}/{} (asked {:?}{})",
        s.sample_rate(),
        s.channel_count(),
        s.format(),
        s.performance_mode(),
        s.sharing_mode(),
        s.frames_per_burst(),
        s.buffer_size_in_frames(),
        s.buffer_capacity_in_frames(),
        live.rung,
        if proven { "" } else { ", UNPROVEN" },
    );
}

/// Walk the ladder until a rung opens, starts, and proves the device is really pulling from it.
///
/// If no rung proves itself, the first one that at least opened and started is reopened and
/// accepted anyway. The watchdog is a heuristic about a device that never calls back, and a
/// heuristic must not be able to turn working audio into no audio at all: a stream that is merely
/// slow to start would otherwise walk the whole ladder and end with the plane disabled, which is
/// strictly worse than the behaviour this function replaced. Reopened rather than held open across
/// the remaining attempts, because a started stream can itself be what makes the next rung fail.
fn open_any(ladder: &[OpenRung], ctx: &OpenCtx) -> Option<LiveStream> {
    let mut unproven: Option<OpenRung> = None;
    for rung in ladder {
        if ctx.shutdown.load(Ordering::Relaxed) {
            return None;
        }
        let live = match try_open(*rung, ctx) {
            Ok(live) => live,
            Err(e) => {
                log::info!("audio: open {rung:?} failed ({e}) — next rung");
                continue;
            }
        };
        match arm(&live, ctx.fmt, ctx.counters, Some(ctx.shutdown)) {
            Ok(()) => {
                log_started(&live, true);
                return Some(live);
            }
            Err(err) => {
                match &err {
                    ArmError::Unusable(why) => {
                        log::warn!("audio: {rung:?} opened but is unusable ({why}) — next rung")
                    }
                    ArmError::NotPulling => {
                        log::warn!(
                            "audio: {rung:?} started but took no samples in {START_WATCHDOG_MS} ms — next rung"
                        );
                        unproven.get_or_insert(*rung);
                    }
                }
                // Ordered teardown before the close in `drop`: the ndk wrapper unwraps
                // AAudioStream_close's status, so hand the HAL a stopped stream.
                let _ = live.stream.request_stop();
            }
        }
    }
    let rung = unproven.filter(|_| !ctx.shutdown.load(Ordering::Relaxed))?;
    log::warn!(
        "audio: no rung proved it was pulling — falling back to {rung:?} unproven; if this device is silent, this line is where to look"
    );
    let live = match try_open(rung, ctx) {
        Ok(l) => l,
        Err(e) => {
            log::error!("audio: unproven {rung:?} did not open either: {e:?} — no playback");
            return None;
        }
    };
    match arm(&live, ctx.fmt, ctx.counters, None) {
        Ok(()) => {
            log_started(&live, false);
            Some(live)
        }
        Err(_) => {
            let _ = live.stream.request_stop();
            None
        }
    }
}

/// Bring one opened stream up and prove the device is really pulling from it. Three failures
/// after a successful `open_stream` each end in silence behind a healthy-looking log:
///
/// 1. **The grant differs from the request.** The callback writes `num_frames * channels` f32
///    into AAudio's buffer, so another layout or format is an out-of-bounds write on a realtime
///    thread. The NDK honours an explicit request or fails the open; this check does not trust
///    the HAL with a buffer length.
/// 2. **`request_start` fails.** The caller tries the next rung.
/// 3. **The stream starts and never calls back.** `prove_pulling` catches it, giving up once
///    that flag is set (a closing session); the last-resort reopen in [`open_any`] passes
///    `None`, since an unproven stream beats no stream.
///
/// The rate is held to the rung, or to the session's rate on an unspecified rung: a device that
/// grants another rate is rejected, never resampled quietly.
fn arm(
    live: &LiveStream,
    fmt: PlaneFormat,
    counters: &Counters,
    prove_pulling: Option<&AtomicBool>,
) -> Result<(), ArmError> {
    let s = &live.stream;
    let channels = fmt.channels;
    let want_rate = live.rung.rate.unwrap_or(fmt.rate_hz as i32);
    if s.channel_count() != channels as i32
        || s.sample_rate() != want_rate
        || s.format() != AudioFormat::PCM_Float
    {
        return Err(ArmError::Unusable(format!(
            "granted rate={} ch={} fmt={:?}, needed {want_rate}/{channels}/PCM_Float",
            s.sample_rate(),
            s.channel_count(),
            s.format(),
        )));
    }
    s.request_start()
        .map_err(|e| ArmError::Unusable(format!("request_start: {e}")))?;
    // Lift the AAudio HW buffer off its brittle ~2-burst LowLatency default so a single late
    // callback doesn't immediately underrun; the in-callback XRun loop grows it further if the
    // device still glitches. set_buffer_size_in_frames clamps to capacity.
    let burst = s.frames_per_burst().max(1);
    let _ = s.set_buffer_size_in_frames((burst * 3).min(s.buffer_capacity_in_frames()));
    let Some(shutdown) = prove_pulling else {
        return Ok(());
    };
    let before = counters.callbacks.load(Ordering::Relaxed);
    let mut waited = 0u64;
    while counters.callbacks.load(Ordering::Relaxed) == before
        && waited < START_WATCHDOG_MS
        && !shutdown.load(Ordering::Relaxed)
    {
        std::thread::sleep(Duration::from_millis(START_WATCHDOG_POLL_MS));
        waited += START_WATCHDOG_POLL_MS;
    }
    if counters.callbacks.load(Ordering::Relaxed) == before {
        return Err(ArmError::NotPulling);
    }
    Ok(())
}

/// Own the audio device for the life of the session: open it, run the decode loop against it, and
/// open it again if AAudio disconnects.
///
/// **Why a supervisor.** AAudio's contract on a disconnect (an HDMI mode switch, an AVR
/// re-handshake, a headset unplugged, a route change) is that the stream is DEAD and the only
/// recovery is close + open a fresh one. This client's error callback logged a warning and did
/// nothing else, so any route change meant silence for the rest of the session while video carried
/// on untouched — and on a TV that is not a rare event, because the client itself drives an HDMI
/// mode switch on the video plane and the platform's own match-content-frame-rate setting drives
/// more.
///
/// The whole plane is rebuilt per generation rather than hot-swapping the channels under the decode
/// loop: reopening costs a few dropped packets once, and a fresh `JitterPolicy` is what you want
/// against a device that may have come back with a different burst size anyway.
fn supervise(
    client: Arc<NativeClient>,
    game_audio: bool,
    is_tv: bool,
    fmt: PlaneFormat,
    shutdown: &AtomicBool,
) {
    // Fold this decode→AAudio thread into the client's hot-thread set so the ADPF session the
    // decode thread opens also keeps audio decode on a fast core (registered before the video
    // pump's first frame arrives, so it's captured when that session is created). No-op below API
    // 33. Done once for the thread, not once per generation — it is the same thread throughout.
    client.register_hot_thread();
    boost_audio_thread("audio");
    let tuning = punktfunk_core::audio::JitterTuning::AAUDIO;
    let counters = Arc::new(Counters::default());
    // The A/V sync hand-off: the realtime callback owns the ring (so it publishes the depth and
    // consumes the target), the decode thread owns the timestamps (so it computes the target).
    // Two atomics, because the callback must not block on the thread that decodes.
    let sync: Arc<punktfunk_core::audio::AudioSyncCell> = Arc::default();
    // Either signal counts. Kotlin's `FEATURE_LEANBACK` is the authoritative one; the sysprop
    // catches a device reached through some path that did not pass the flag, and neither answering
    // simply keeps the phone ladder.
    let ladder = open_ladder(is_tv || is_tv_device(), fmt);
    // The one line that says what this session actually resolved — the `Welcome`'s answer, not the
    // request. A report of "hi-res is on but it sounds the same" is triaged from here: `codec=0`
    // means the host declined and the session is ordinary Opus, and the host's own log says why.
    log::info!(
        "audio: plane codec={} rate={} bits={} ch={} frame_us={} — open ladder {ladder:?}",
        fmt.codec,
        fmt.rate_hz,
        fmt.bits,
        fmt.channels,
        fmt.frame_us,
    );
    // An escape hatch for the reopen itself: if reopening ever turns out to fight a device (a HAL
    // that disconnects in a loop), the field can pin the old give-up-on-disconnect behaviour
    // without a rebuild rather than living with a restart storm.
    let reopen_allowed = !matches!(
        sysprop(c"debug.punktfunk.audio_reopen").as_deref(),
        Some("0") | Some("false")
    );

    let mut generation: u32 = 0;
    let mut reopen_attempt: u32 = 0;
    while !shutdown.load(Ordering::Relaxed) {
        let disconnected = Arc::new(AtomicBool::new(false));
        let ctx = OpenCtx {
            fmt,
            tuning,
            // Worst transient the ring can hold before the policy trims it. Through the format's
            // own conversion rather than a samples-per-millisecond constant: the cap is a hard
            // ceiling on latency, and one computed 2.3 % shallow on a 44.1-family session would
            // trim a ring that was inside its budget.
            hard_cap_max: fmt.ms_samples(tuning.hard_cap_ms),
            game_audio,
            counters: &counters,
            sync: &sync,
            disconnected: &disconnected,
            shutdown,
        };
        let live = match open_any(&ladder, &ctx) {
            Some(live) => {
                reopen_attempt = 0;
                live
            }
            None if shutdown.load(Ordering::Relaxed) => return,
            // A reopen that lands in the middle of the very route change that caused the
            // disconnect finds no usable device and would otherwise disable audio permanently —
            // the exact outcome this supervisor exists to prevent. An HDMI mode switch or an AVR
            // re-handshake takes a beat, so keep trying across it before giving up.
            None if generation > 0 && reopen_attempt < REOPEN_ATTEMPTS => {
                reopen_attempt += 1;
                log::warn!(
                    "audio: reopen attempt {reopen_attempt}/{REOPEN_ATTEMPTS} found no usable configuration — retrying"
                );
                nap(shutdown, REOPEN_SETTLE_MS);
                continue;
            }
            None => {
                log_no_configuration(fmt);
                return;
            }
        };
        if generation > 0 {
            log::info!("audio: reopened after disconnect (generation {generation})");
        }
        let exit = decode_loop(
            &client,
            &live,
            shutdown,
            &disconnected,
            &counters,
            fmt,
            &sync,
        );
        let _ = live.stream.request_stop();
        drop(live); // → AAudioStream_close
        match exit {
            DecodeExit::Disconnected if reopen_allowed && !shutdown.load(Ordering::Relaxed) => {
                generation += 1;
                log::warn!("audio: device disconnected — reopening in {REOPEN_SETTLE_MS} ms");
                nap(shutdown, REOPEN_SETTLE_MS);
            }
            DecodeExit::Disconnected => {
                log::warn!("audio: device disconnected — not reopening (shutting down, or pinned by debug.punktfunk.audio_reopen)");
                break;
            }
            DecodeExit::Shutdown | DecodeExit::SessionClosed | DecodeExit::Fatal => break,
        }
    }
    log::info!(
        "audio: stopped ({}={} pcm_frames={} underruns={} generations={})",
        plane_counter_key(fmt),
        counters.frames_decoded.load(Ordering::Relaxed),
        counters.pcm_written.load(Ordering::Relaxed),
        counters.underruns.load(Ordering::Relaxed),
        generation + 1,
    );
}

/// What the decoded-frame counter is called in the log lines. Kept plane-specific rather than
/// renamed to something neutral so that the `opus=` an existing field report or triage note greps
/// for still means exactly what it always meant — and a lossless session is visibly a different
/// line rather than the same one with a surprising rate.
fn plane_counter_key(fmt: PlaneFormat) -> &'static str {
    if fmt.is_pcm() {
        "pcm"
    } else {
        "opus"
    }
}

/// `ANDROID_PRIORITY_AUDIO`: Android gives app threads no SCHED_FIFO, and -16 is what the
/// platform's own audio threads run at.
pub(crate) const AUDIO_NICE: i32 = -16;

/// Raise the calling audio or mic thread to [`AUDIO_NICE`], every session.
pub(crate) fn boost_audio_thread(who: &str) {
    if let Err(e) = crate::sys::set_thread_nice(None, AUDIO_NICE) {
        log::debug!("{who}: setpriority({AUDIO_NICE}) refused (non-fatal): {e}");
    }
}

/// Sleep up to `total_ms`, in slices, giving up early once `shutdown` is set.
///
/// The supervisor's backoffs run on the same thread `AudioPlayback::drop` joins, so a plain
/// `sleep` would make closing a session wait out a reopen backoff — up to two seconds of it with
/// [`REOPEN_ATTEMPTS`] in play. Teardown latency is a user-visible thing; a settling delay is not
/// worth spending it.
pub(crate) fn nap(shutdown: &AtomicBool, total_ms: u64) {
    const SLICE_MS: u64 = 25;
    let mut left = total_ms;
    while left > 0 && !shutdown.load(Ordering::Relaxed) {
        let slice = left.min(SLICE_MS);
        std::thread::sleep(Duration::from_millis(slice));
        left -= slice;
    }
}

/// The realtime consumer, owned by the AAudio data callback (FnMut) — no lock: AAudio calls it
/// from a single high-priority thread, and the decode thread only touches `tx`/`free_rx`.
struct Render {
    counters: Arc<Counters>,
    sync: Arc<punktfunk_core::audio::AudioSyncCell>,
    rx: Receiver<Vec<f32>>,
    free_tx: SyncSender<Vec<f32>>,
    channels: usize,
    ring: VecDeque<f32>,
    policy: punktfunk_core::audio::JitterPolicy,
    /// Callbacks since open (throttles the XRun grow check).
    cb_count: u32,
    /// Last AAudio XRun count we grew the buffer for.
    last_xrun: i32,
}

impl Render {
    fn new(ctx: &OpenCtx, rx: Receiver<Vec<f32>>, free_tx: SyncSender<Vec<f32>>) -> Render {
        let OpenCtx {
            fmt,
            tuning,
            hard_cap_max,
            ..
        } = *ctx;
        let channels = usize::from(fmt.channels);
        // Pre-reserve the ring so `extend` never reallocates on the realtime thread. Worst
        // transient before a trim = the hard cap plus one full channel of the plane's OWN frame,
        // sized from the resolved format — rate, depth AND channel count — so a surround
        // session's longer-per-frame chunks never force a one-time realloc on the RT thread
        // (asserted in `decode_loop`).
        let ring = VecDeque::with_capacity(hard_cap_max + RING_CHUNKS * fmt.frame_samples());
        // The de-jitter policy is told the RESOLVED format on both axes: depths are milliseconds
        // converted to samples at the session's rate (multiplying first, so 44.1 kHz stays
        // exact), and the target floor plus the smooth shed are denominated in FRAMES, so a 2 ms
        // lossless frame is shed and crossfaded as one frame. Microseconds: sub-ms rungs exist.
        let mut policy =
            punktfunk_core::audio::JitterPolicy::new_at_rate(tuning, fmt.channels, fmt.rate_hz);
        policy.set_frame_us(fmt.frame_us);
        Render {
            counters: ctx.counters.clone(),
            sync: ctx.sync.clone(),
            rx,
            free_tx,
            channels,
            ring,
            policy,
            cb_count: 0,
            last_xrun: 0,
        }
    }

    fn callback(
        &mut self,
        s: &AudioStream,
        data: *mut c_void,
        num_frames: i32,
    ) -> AudioCallbackResult {
        // Proof of life for `arm`'s start watchdog, and the one counter that separates
        // "the device never pulled" from "the device pulled silence": bumped before any
        // early-out, primed or not.
        self.counters.callbacks.fetch_add(1, Ordering::Relaxed);
        let Some(want) = crate::audio_format::callback_sample_count(num_frames, self.channels)
        else {
            return AudioCallbackResult::Continue;
        };
        if data.is_null() {
            return AudioCallbackResult::Stop;
        }
        // SAFETY: AAudio provides `num_frames * channel_count` f32 slots at non-null `data`.
        // `arm` verified the granted channel count and PCM_Float format; checked arithmetic above
        // rejected nonpositive or unrepresentable lengths before this slice was formed.
        let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<f32>(), want) };
        self.fill(out, num_frames);
        self.grow_on_xrun(s);
        AudioCallbackResult::Continue
    }

    /// Drain the decoded chunks into the ring, run the jitter policy, and hand AAudio `out`.
    fn fill(&mut self, out: &mut [f32], num_frames: i32) {
        // Drain WITHOUT freeing on the RT thread: `drain(..)` empties each Vec but keeps its
        // capacity, then the empty buffer is handed back for reuse. The only RT-thread free is
        // the rare case where the recycle channel is momentarily full.
        while let Ok(mut chunk) = self.rx.try_recv() {
            self.ring.extend(chunk.drain(..));
            let _ = self.free_tx.try_send(chunk);
        }
        // A/V sync: take whatever depth the decode thread's sync loop last asked for, and
        // publish where the ring actually is so it can measure the result. The policy clamps
        // the request between its own underrun floor and the hard cap — continuity outranks
        // sync. Read AFTER the drain, so the depth is everything a frame queued now waits behind.
        self.policy.set_sync_target(self.sync.target());
        self.sync.publish_depth(self.ring.len());
        // The policy decides prime/silence, trims a burst, sheds ONE crossfaded frame when the
        // depth has sat above target long enough to be drift, and — the mirror — duplicates one
        // frame when the sync loop asked for a DEEPER ring. Inserts stay inside the ring's
        // reserve: the policy only inserts below its target.
        let step = self.policy.step(self.ring.len(), out.len());
        if step.drop_front > 0 {
            punktfunk_core::audio::crossfade_drop(&mut self.ring, step.drop_front, step.crossfade);
        }
        if step.insert_front > 0 {
            punktfunk_core::audio::crossfade_insert(
                &mut self.ring,
                step.insert_front,
                step.crossfade,
            );
            self.counters.inserts.fetch_add(1, Ordering::Relaxed);
        }
        let mut ran_short = false;
        if !step.silence {
            for slot in out.iter_mut() {
                *slot = self.ring.pop_front().unwrap_or_else(|| {
                    ran_short = true;
                    0.0
                });
            }
            self.counters
                .pcm_written
                .fetch_add(num_frames as u64, Ordering::Relaxed);
        } else {
            out.fill(0.0);
            self.counters.underruns.fetch_add(1, Ordering::Relaxed);
        }
        // No-op while un-primed, so a deliberate priming silence is never counted as an
        // underrun (which would otherwise drive the adaptive floor up for no reason).
        self.policy.note_read(ran_short);
        self.counters
            .target_ms
            .store(self.policy.target_ms() as u64, Ordering::Relaxed);
    }

    /// Google's AAudio anti-glitch technique: when the device reports new XRuns, grow the HW
    /// buffer by one burst (up to capacity). Both calls are callback-safe and `set` clamps to
    /// capacity, so it self-limits. Throttled to every `XRUN_CHECK_EVERY` callbacks.
    fn grow_on_xrun(&mut self, s: &AudioStream) {
        self.cb_count = self.cb_count.wrapping_add(1);
        if self.cb_count % XRUN_CHECK_EVERY != 0 {
            return;
        }
        let xr = s.x_run_count();
        if xr > self.last_xrun {
            self.last_xrun = xr;
            let burst = s.frames_per_burst().max(1);
            let grown = (s.buffer_size_in_frames() + burst).min(s.buffer_capacity_in_frames());
            let _ = s.set_buffer_size_in_frames(grown);
        }
    }
}

/// Every rung refused. Name the format, because at 96 kHz it is the likeliest cause and the
/// cure is a setting rather than a rebuild. Near-unreachable — `connect` proves the rate is
/// openable BEFORE the `Hello` — so getting here on a hi-res session means the device changed
/// underneath one that did open.
fn log_no_configuration(fmt: PlaneFormat) {
    log::error!(
        "audio: no AAudio configuration on the ladder could be opened and started at {} Hz / {} ch — audio disabled for this session (video unaffected){}",
        fmt.rate_hz,
        fmt.channels,
        if fmt.rate_hz == punktfunk_core::audio::SAMPLE_RATE_HZ {
            ""
        } else {
            "; this device would not give us the rate the host resolved — turn hi-res audio off in Settings to run this session at 48 kHz"
        },
    );
}

/// Open one ladder rung with fresh callback state.
///
/// `open_stream` consumes its builder and callback, so channels, buffering, and jitter policy are
/// rebuilt on every attempt. Invalid callback pointer/length pairs stop before a slice is formed.
fn try_open(rung: OpenRung, ctx: &OpenCtx) -> ndk::audio::Result<LiveStream> {
    let OpenCtx {
        fmt, game_audio, ..
    } = *ctx;
    let channels = fmt.channels;
    let (tx, rx) = sync_channel::<Vec<f32>>(RING_CHUNKS);
    // Recycle free-list: drained PCM buffers go BACK to the decode thread to be refilled, so
    // the realtime callback never frees heap (Android's Scudo allocator has unbounded free()
    // tail latency — a free on the audio thread is an XRun = a click) and the decode thread
    // rarely allocates. Same depth as the data channel.
    let (free_tx, free_rx) = sync_channel::<Vec<f32>>(RING_CHUNKS);

    let mut render = Render::new(ctx, rx, free_tx);
    let callback = move |s: &AudioStream, data: *mut c_void, num_frames: i32| {
        render.callback(s, data, num_frames)
    };

    let builder = AudioStreamBuilder::new()?
        .direction(AudioDirection::Output)
        // The wire order (FL FR FC LFE RL RR SL SR) is the standard AAudio/Android channel
        // order, so this is an IDENTITY mapping — no permute. AAudio infers the 5.1/7.1 mask
        // from `channel_count` (the ndk crate's builder exposes no setChannelMask); the host
        // captures + encodes in exactly this order.
        .channel_count(channels as i32)
        // ⚠ The DEVICE format is f32 on BOTH planes, deliberately — this is not an oversight
        // left over from the Opus-only era. Core decodes each plane to interleaved f32 (libopus
        // `decode_float`; `pcm::to_f32` normalises 16/24-bit codes by their full scale), so a
        // 24-bit session already arrives as floats and asking AAudio for PCM_I24_PACKED would
        // mean quantising them BACK — a second rounding, of the very samples the plane exists to
        // deliver unrounded. It would also be unreachable here: that format is API 31, above this
        // client's minSdk-28 floor. The wire depth is a WIRE fact; it never reaches the HAL.
        .format(AudioFormat::PCM_Float);
    // The rate is per-rung: an explicit request (honoured or the open fails — AAudio never
    // substitutes silently) or, on the last rate rung, nothing at all, letting the HAL name its
    // own. `arm` holds an unspecified rung to the session's rate afterwards, so "let AAudio
    // choose" can rescue a stubborn HAL but can never quietly change what we are playing.
    let builder = match rung.rate {
        Some(hz) => builder.sample_rate(hz),
        None => builder,
    };
    // Tag the stream as game audio (usage=Game / content=Movie): the audio HAL applies
    // its low-latency game-audio routing/policy and it's grouped correctly with the
    // game-mode profile. Advisory — ignored where the device has no such policy. Part of
    // the experimental low-latency stack; off, the stream stays untagged.
    let builder = if game_audio {
        builder
            .usage(AudioUsage::Game)
            .content_type(AudioContentType::Movie)
    } else {
        builder
    };
    // AAudio calls the error callback on its own thread (never the realtime one), and its
    // contract is that the stream is finished: the ONLY recovery is close + open a new
    // one, which is what setting this flag asks `supervise` to do. Doing it here would
    // deadlock — you may not close a stream from inside its own callbacks.
    let cb_disconnected = ctx.disconnected.clone();
    let stream = builder
        .performance_mode(rung.perf)
        .sharing_mode(rung.sharing)
        .data_callback(Box::new(callback))
        .error_callback(Box::new(move |_s, e| {
            log::warn!("audio: AAudio error (device reroute/disconnect?): {e:?}");
            cb_disconnected.store(true, Ordering::SeqCst);
        }))
        .open_stream()?;
    Ok(LiveStream {
        stream,
        tx,
        free_rx,
        rung,
    })
}

/// Producer: `next_audio` → decode (libopus, or a PCM stride unpack) → push interleaved f32 into
/// the ring channel. Buffers come from (and return to) the realtime callback's recycle free-list so
/// the steady state is allocation-free on both threads.
///
/// Runs on the supervisor's thread and returns when the session ends, the playback is dropped, or
/// the device disconnects — [`DecodeExit`] says which, because only one of them is worth reopening
/// the device for.
fn decode_loop(
    client: &Arc<NativeClient>,
    live: &LiveStream,
    shutdown: &AtomicBool,
    disconnected: &AtomicBool,
    counters: &Counters,
    fmt: PlaneFormat,
    sync: &punktfunk_core::audio::AudioSyncCell,
) -> DecodeExit {
    let mut plane = match Plane::new(client, live, counters, fmt, sync) {
        Ok(p) => p,
        Err(exit) => return exit,
    };
    // One tick = one frame of THIS plane, which is what makes the drought arm conceal at the rate
    // the callback drains at rather than racing it or falling behind: 5 ms on Opus, as little as
    // 1 ms on a `0xD3` session at the short end of the ladder. Also the poll period for both exit
    // flags, so a disconnect is still noticed within one packet time on a silent link.
    let tick = Duration::from_micros(fmt.frame_us.max(1) as u64);
    while !shutdown.load(Ordering::Relaxed) {
        if disconnected.load(Ordering::Relaxed) {
            return DecodeExit::Disconnected;
        }
        let step = match client.next_audio(tick) {
            Ok(pkt) => plane.on_packet(&pkt),
            Err(PunktfunkError::NoFrame) => plane.on_quiet(),
            Err(_) => return DecodeExit::SessionClosed,
        };
        if let Err(exit) = step {
            return exit;
        }
    }
    DecodeExit::Shutdown
}

/// One decoding audio plane: the decoder and its scratch, the gap + drought concealment, the
/// A/V placement, and the ring it pushes into.
struct Plane<'a> {
    /// Read per push for the mute mask — a local mute zeroes what is queued, never what is
    /// decoded, so the decoder keeps its state and unmute lands in step.
    client: &'a NativeClient,
    live: &'a LiveStream,
    counters: &'a Counters,
    fmt: PlaneFormat,
    sync: &'a punktfunk_core::audio::AudioSyncCell,
    dec: PlaneDecoder,
    pcm: Vec<f32>,
    channels: usize,
    /// Per-channel samples of the last decoded frame — the PLC unit. 0 until something has
    /// decoded: there is no state to extrapolate from before then.
    frame_samples: usize,
    /// Loudest |sample| since the last log — tells a tone from silence.
    window_peak: f32,
    gaps: punktfunk_core::audio::AudioGapTracker,
    drought: punktfunk_core::audio::DroughtConceal,
    last_packet: std::time::Instant,
    av_sync_enabled: bool,
    av: punktfunk_core::audio::AvSync,
    video_e2e: Arc<AtomicU64>,
    av_offset_out: Arc<std::sync::atomic::AtomicI64>,
    buffer_ms_out: Arc<std::sync::atomic::AtomicU32>,
    /// The dead-stream warning is a DURATION, not a packet count — see `DEAD_STREAM_WARN_MS`.
    dead_stream_warn_packets: u64,
}

impl<'a> Plane<'a> {
    fn new(
        client: &'a NativeClient,
        live: &'a LiveStream,
        counters: &'a Counters,
        fmt: PlaneFormat,
        sync: &'a punktfunk_core::audio::AudioSyncCell,
    ) -> Result<Plane<'a>, DecodeExit> {
        let channels = usize::from(fmt.channels);
        // An unknown Opus coupling is refused like any other init failure: a guessed pairing
        // plays the wrong speakers.
        let dec = PlaneDecoder::new(&fmt).map_err(|e| {
            log::error!(
                "audio: decoder init for codec={} rate={} ch={}: {e} — audio disabled",
                fmt.codec,
                fmt.rate_hz,
                channels,
            );
            DecodeExit::Fatal
        })?;
        // Everything denominated in FRAMES is told this plane's frame: `MAX_CONCEAL_MS` caps a
        // single loss event at 50 ms of synthesized audio and derives the packet count from it;
        // the drought's wall-clock fuse and `plc_ms` are spent at the rate this session paces.
        let mut gaps = punktfunk_core::audio::AudioGapTracker::new();
        gaps.set_frame_us(fmt.frame_us);
        let drought = punktfunk_core::audio::DroughtConceal::new_at_frame_us(
            punktfunk_core::audio::JitterTuning::AAUDIO.plc_max_ms(),
            fmt.frame_us,
        );
        // A/V sync: this thread is the only place holding all three ingredients at once — the
        // packet's host capture `pts_ns`, the ring depth (via the sync cell) and the video plane's
        // end-to-end figure. At the RESOLVED rate, for the same reason `JitterPolicy` is: the
        // proposal is denominated in the ring's own samples-per-millisecond.
        let av_sync_enabled = av_sync_enabled();
        if !av_sync_enabled {
            log::info!(
                "audio: A/V sync disabled (PUNKTFUNK_NO_AV_SYNC / debug.punktfunk.no_av_sync)"
            );
        }
        Ok(Plane {
            client,
            live,
            counters,
            fmt,
            sync,
            dec,
            // The largest frame this plane can carry; the decoder clamps into it.
            pcm: vec![0f32; fmt.max_frame_samples()],
            channels,
            frame_samples: 0,
            window_peak: 0.0,
            gaps,
            drought,
            last_packet: std::time::Instant::now(),
            av_sync_enabled,
            av: {
                let mut av = punktfunk_core::audio::AvSync::new_at_rate(fmt.channels, fmt.rate_hz);
                av.set_frame_us(fmt.frame_us);
                av
            },
            video_e2e: client.video_e2e_shared(),
            av_offset_out: client.audio_av_offset_shared(),
            buffer_ms_out: client.audio_buffer_ms_shared(),
            dead_stream_warn_packets: (DEAD_STREAM_WARN_MS * 1000 / fmt.frame_us.max(1) as u64)
                .max(1),
        })
    }

    /// Hand `pcm[..n]` to the ring in a recycled buffer (allocating only when the free-list is
    /// momentarily empty: startup / after a backpressure drop). Full = drop-newest. A muted
    /// stream queues the same length in silence, so the ring keeps its cadence and the device
    /// is never closed and reopened on a toggle.
    fn push(&mut self, n: usize) -> Result<(), DecodeExit> {
        let mut buf = self
            .live
            .free_rx
            .try_recv()
            .unwrap_or_else(|_| Vec::with_capacity(self.pcm.len()));
        buf.clear();
        if self.client.audio_muted() {
            buf.resize(n, 0.0);
        } else {
            buf.extend_from_slice(&self.pcm[..n]);
        }
        match self.live.tx.try_send(buf) {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => Err(DecodeExit::Shutdown),
        }
    }

    /// One synthesized frame into the ring: an inaudible fade instead of the click a hard gap
    /// makes. libopus interpolates from its own state on `0xC9`; `0xD3` has none to interpolate
    /// from, so `PcmConceal` repeats-and-fades and decays a run to silence (§4.5). `Ok(false)` =
    /// nothing to build from — let the ring carry the gap.
    fn conceal_one(&mut self) -> Result<bool, DecodeExit> {
        if self.frame_samples == 0 {
            return Ok(false);
        }
        match self.dec.conceal(self.frame_samples, &mut self.pcm) {
            Ok(samples @ 1..) => self.push(samples * self.channels).map(|()| true),
            _ => Ok(false),
        }
    }

    /// Device output latency past the ring, ns: when AAudio will play the newest frame it was
    /// handed, less now. `0` until the stream reports a timestamp (it has just started).
    fn output_latency_ns(&self) -> u64 {
        let stream = &self.live.stream;
        let Ok(ts) = stream.timestamp(ndk::audio::Clockid::Monotonic) else {
            return 0;
        };
        let ahead = stream.frames_written() - ts.frame_position;
        let heard_at =
            ts.time_nanoseconds + ahead * 1_000_000_000 / i64::from(self.fmt.rate_hz.max(1));
        (heard_at - crate::sys::now_monotonic_ns()).max(0) as u64
    }

    /// Place the packet against the picture, conceal any seq gap in front of it, decode it into
    /// the ring, and keep the 1 Hz line. An empty payload decodes nothing.
    fn on_packet(&mut self, pkt: &AudioPacket) -> Result<(), DecodeExit> {
        // BEFORE it is queued: `buffered_ahead` is everything that must still play first, so
        // the depth read here is exactly what delays it. Published unconditionally — the ring's
        // depth is what makes a "the audio delay is way too high" report triageable at all.
        // Converted through the format, never by an integer samples-per-millisecond divisor
        // (44.1 at 44 100 Hz reported a healthy ring 2.3 % DEEP).
        let depth = self.sync.depth();
        self.buffer_ms_out
            .store(self.fmt.samples_ms(depth), Ordering::Relaxed);
        if self.av_sync_enabled {
            let ve2e = self.video_e2e.load(Ordering::Relaxed);
            self.av.observe(punktfunk_core::audio::AvSyncObservation {
                pts_ns: pkt.pts_ns,
                now_local_ns: punktfunk_core::client::now_realtime_ns(),
                clock_offset_ns: self.client.clock_offset_now_ns(),
                buffered_ahead: depth,
                output_latency_ns: self.output_latency_ns(),
                // 0 = nothing confirmed on the glass yet (no render callback below API 33, or
                // the stream has not presented a frame); no reference, no correction.
                video_e2e_ns: (ve2e > 0).then_some(ve2e),
            });
            self.sync.set_target(self.av.desired_depth(depth));
            self.av_offset_out
                .store(self.av.offset_ms() as i64, Ordering::Relaxed);
        }
        self.last_packet = std::time::Instant::now();
        // Anything the drought path already covered is audio the stream now has; concealing it
        // a second time would insert samples it never carried and push everything after later.
        let already = self.drought.packet();
        for _ in 0..self.gaps.missing_before(pkt.seq).saturating_sub(already) {
            if !self.conceal_one()? {
                break;
            }
        }
        let samples = match self.dec.decode(&pkt.data, &mut self.pcm) {
            // Empty payload: the last frame stays the concealment unit.
            Ok(0) => return Ok(()),
            Ok(s) => s,
            Err(e) => {
                log::debug!("audio: decode: {e}");
                return Ok(());
            }
        };
        self.frame_samples = samples;
        let n = samples * self.channels;
        for &s in &self.pcm[..n] {
            self.window_peak = self.window_peak.max(s.abs());
        }
        // The ring's pre-reservation in `try_open` is one frame of THIS plane per queued chunk; a
        // larger frame would force a one-time realloc on the RT thread. Catch a host that changed
        // its frame size — or a `Welcome` whose `audio_frame_us` disagrees with what it sends.
        debug_assert!(
            n <= self.fmt.frame_samples(),
            "audio frame {n} f32 exceeds the {} f32 ring reserve ({} µs at {} Hz)",
            self.fmt.frame_samples(),
            self.fmt.frame_us,
            self.fmt.rate_hz,
        );
        let count = self.counters.frames_decoded.fetch_add(1, Ordering::Relaxed) + 1;
        self.push(n)?;
        // The fingerprint of a stream that opened into a device which is not playing: decoding
        // steadily and AAudio has never taken a single sample. `arm`'s watchdog catches it at
        // open, so reaching this means the device stopped pulling AFTER it started — from the
        // outside indistinguishable from "the app has no sound".
        if count == self.dead_stream_warn_packets
            && self.counters.pcm_written.load(Ordering::Relaxed) == 0
        {
            log::error!(
                "audio: {count} frames decoded ({DEAD_STREAM_WARN_MS} ms) but AAudio has not taken one sample — {:?} opened into a device that is not playing",
                self.live.rung,
            );
        }
        if count % 600 == 0 {
            // `av_ms` is the smoothed placement error (+ = audio behind the picture; 0 with
            // sync off or no video reference) — a deep ring on a jittery link is correct, and
            // only the offset separates that from audio held late. `plc_ms` is drought
            // concealment: healthy `underruns` bought with a climbing `plc_ms` is a link in trouble.
            log::info!(
                "audio: {}={count} pcm_frames={} underruns={} buffer_ms={} target_ms={} av_ms={} plc_ms={} drift_inserts={} peak={:.3}",
                plane_counter_key(self.fmt),
                self.counters.pcm_written.load(Ordering::Relaxed),
                self.counters.underruns.load(Ordering::Relaxed),
                self.fmt.samples_ms(depth),
                self.counters.target_ms.load(Ordering::Relaxed),
                self.av.offset_ms(),
                self.drought.total_ms(),
                self.counters.inserts.load(Ordering::Relaxed),
                self.window_peak,
            );
            self.window_peak = 0.0;
        }
        Ok(())
    }

    /// Nothing on the wire this tick. If the ring is draining with it, conceal — the same
    /// synthesis the loss path uses, bounded by this preset's de-prime fuse so a genuinely dead
    /// stream is not papered over. ONE frame per tick, not a burst: the tick is one frame of this
    /// plane and therefore the rate the callback drains at, so concealment keeps pace with playout
    /// instead of racing ahead of a depth reading it has already invalidated.
    fn on_quiet(&mut self) -> Result<(), DecodeExit> {
        let depth_ms = self.fmt.samples_ms(self.sync.depth());
        if self.frame_samples > 0 && self.drought.conceal(self.last_packet.elapsed(), depth_ms) {
            self.conceal_one()?;
            self.sync.publish_plc_ms(self.drought.total_ms());
        }
        Ok(())
    }
}
