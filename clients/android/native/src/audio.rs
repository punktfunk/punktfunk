//! Android audio playback: decode the negotiated Opus (`0xC9`) or lossless PCM (`0xD3`)
//! plane to interleaved f32 and feed AAudio through a jitter ring.
//!
//! [`PlaneFormat`] fixes rate, depth, frame size, and the host-resolved stereo/5.1/7.1
//! layout for the session. [`ladder_for`] lists viable AAudio modes at that exact rate,
//! [`arm`] verifies that callbacks start, and [`supervise`] reopens disconnected devices.
//! Unsupported rates are rejected before `Hello` by [`output_rate_is_openable`]; playback
//! never silently resamples negotiated audio.
//!
//! The realtime callback recycles buffers rather than allocating. The shared
//! `JitterTuning::AAUDIO` policy handles drift and underruns, while `DisplayTracker` supplies
//! the video reference for A/V sync. See `design/hi-res-audio.md` and
//! `design/audio-latency-overhaul.md` for the full policy.

use crate::audio_policy::{
    grant_matches, heard_at, ladder_for, sent, DecodeExit, Next, NoDevice, OpenRung, Perf, Sharing,
    Supervisor, REOPEN_ATTEMPTS,
};
use crate::sys::sysprop;
use ndk::audio::{
    AudioCallbackResult, AudioContentType, AudioDirection, AudioFormat, AudioPerformanceMode,
    AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage,
};
use punktfunk_core::audio::plane::{PlaneDecoder, PlaneFormat};
use punktfunk_core::audio::AudioPlayout;
use punktfunk_core::client::{AudioPacket, NativeClient};
use punktfunk_core::error::PunktfunkError;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// What one playback open attempt yields: the output, plus both halves of the PCM hand-off — the
/// sender the decode thread fills and the receiver that returns drained buffers for refill.
///
/// A named struct rather than a tuple, because the tuple's type tripped `clippy::type_complexity`
/// and because the open path carries the rung it succeeded on into the logs.
struct LiveStream {
    output: Output,
    tx: SyncSender<Vec<f32>>,
    free_rx: Receiver<Vec<f32>>,
    rung: OpenRung,
}

/// The device a session plays into. Dropping it closes the stream or releases the track.
enum Output {
    AAudio(AudioStream),
    /// Surround below API 32, where AAudio has no speaker mask: [`crate::audio_track`].
    Track(crate::audio_track::Track),
}

impl Output {
    /// `(rate, channels, f32)` as granted, for [`arm`]'s check against the session.
    fn granted(&self) -> (i32, i32, bool) {
        match self {
            Output::AAudio(s) => (
                s.sample_rate(),
                s.channel_count(),
                s.format() == AudioFormat::PCM_Float,
            ),
            Output::Track(t) => (t.granted.rate_hz, t.granted.channels, true),
        }
    }

    /// Ordered teardown before the close in `drop`: the ndk wrapper unwraps
    /// AAudioStream_close's status, so hand the HAL a stopped stream. A track stops in its drop.
    fn stop(&self) {
        if let Output::AAudio(s) = self {
            let _ = s.request_stop();
        }
    }

    /// When the newest frame handed over plays, less now, ns; `0` before the first timestamp.
    fn latency_ns(&self, rate_hz: u32) -> u64 {
        let heard_at = match self {
            Output::AAudio(s) => {
                let Ok(ts) = s.timestamp(ndk::audio::Clockid::Monotonic) else {
                    return 0;
                };
                heard_at(
                    ts.time_nanoseconds,
                    ts.frame_position,
                    s.frames_written(),
                    rate_hz,
                )
            }
            Output::Track(t) => match t.heard_at_ns() {
                0 => return 0,
                at => at,
            },
        };
        (heard_at - crate::sys::now_monotonic_ns()).max(0) as u64
    }
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

/// The sysprop half of the TV test for [`ladder_for`]: `ro.build.characteristics` lists `tv` on
/// every Android TV build. The caller ORs it with Kotlin's `FEATURE_LEANBACK`; an unset property
/// reads as not a TV.
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
    /// [`ladder_for`], runs the decode loop against it, and reopens it if the device disconnects.
    /// `None` only if the thread itself could not be spawned — an open failure is reported by the
    /// supervisor (the caller leaves video streaming either way).
    ///
    /// `game_audio` (the experimental low-latency mode) tags the stream usage=Game for the HAL's
    /// game-audio routing; off, the stream is untagged. `is_tv` is Kotlin's `FEATURE_LEANBACK`
    /// and steers the ladder.
    pub fn start(
        client: Arc<NativeClient>,
        game_audio: bool,
        is_tv: bool,
    ) -> Option<AudioPlayback> {
        // Everything about the format comes from what the host RESOLVED, never from what this
        // device asked for: the channel count (2 = stereo / 6 = 5.1 / 8 = 7.1, canonical wire
        // order FL FR FC LFE RL RR SL SR), the plane, the rate, the depth and the frame duration.
        let fmt = client.audio_format();
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

// void AAudioStreamBuilder_setChannelMask(AAudioStreamBuilder*, aaudio_channel_mask_t)  (API 32)
type SetChannelMaskFn = unsafe extern "C" fn(*mut c_void, u32);

/// `AAudioStreamBuilder_setChannelMask`, resolved once; `None` below API 32.
fn set_channel_mask_fn() -> Option<SetChannelMaskFn> {
    static SET_MASK: OnceLock<Option<SetChannelMaskFn>> = OnceLock::new();
    *SET_MASK.get_or_init(|| {
        // SAFETY: the `ndk` audio wrapper links `libaaudio.so`, so `dlopen` only bumps its
        // refcount (process-lifetime handle; null is checked). The type is the NDK header's.
        unsafe {
            let lib = libc::dlopen(c"libaaudio.so".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                None
            } else {
                crate::sym::<SetChannelMaskFn>(lib, c"AAudioStreamBuilder_setChannelMask")
            }
        }
    })
}

/// `builder` set to the wire layout: the channel count, plus AAudio's speaker mask above stereo.
///
/// From API 29 a count alone opens more than two channels as an index mask, with no speaker
/// positions, so a stereo output keeps FL/FR and drops the rest. AAudio's mask bits are WASAPI's,
/// in wire order. API 29–31 have no mask, and surround plays through [`crate::audio_track`].
///
/// Surround also takes a session ID, which AAudio serves only off MMAP. A shared MMAP endpoint
/// opens at its first stream's layout and leaves a 5.1 mix to the HAL; off MMAP, AudioFlinger
/// downmixes it or routes it to a multichannel output.
fn with_layout(builder: AudioStreamBuilder, channels: u8) -> AudioStreamBuilder {
    let builder = builder.channel_count(i32::from(channels));
    if channels <= 2 {
        return builder;
    }
    let builder = builder.session_id(None);
    if let Some(set_mask) = set_channel_mask_fn() {
        // SAFETY: the pointer is the live builder `builder` owns. A mask set after the count wins.
        unsafe {
            set_mask(
                builder.as_ptr().cast(),
                punktfunk_core::audio::wasapi_channel_mask(channels),
            )
        };
    }
    builder
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
/// the host-resolved layout does not exist until `Welcome`. It carries playback's speaker mask:
/// a 5.1 mask can route to another output than a bare count, at another rate.
pub fn output_rate_is_openable(rate_hz: u32, channels: u8) -> bool {
    let built = AudioStreamBuilder::new().map(|b| {
        let channels = punktfunk_core::audio::normalize_channels(channels);
        with_layout(b.direction(AudioDirection::Output), channels)
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

/// Everything an open attempt needs that does not vary between rungs.
struct OpenCtx<'a> {
    /// The session's resolved format — what the ring is sized in and what [`arm`] holds an
    /// unspecified-rate rung to.
    fmt: PlaneFormat,
    /// The session's playout; every open builds its callback's policy from it.
    playout: AudioPlayout,
    hard_cap_max: usize,
    game_audio: bool,
    counters: &'a Arc<Counters>,
    sync: &'a Arc<punktfunk_core::audio::AudioSyncCell>,
    /// Set by the AAudio error callback or a failed track write when the device goes — see
    /// [`supervise`].
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
    let s = match &live.output {
        Output::AAudio(s) => s,
        Output::Track(t) => {
            let g = t.granted;
            log::info!(
                "audio: AudioTrack started rate={} ch={} perf={} buf={}/{} (asked {:?}{})",
                g.rate_hz,
                g.channels,
                g.perf,
                g.buffer_frames,
                g.capacity_frames,
                live.rung,
                if proven { "" } else { ", UNPROVEN" },
            );
            return;
        }
    };
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
                live.output.stop();
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
            live.output.stop();
            None
        }
    }
}

/// Bring one opened stream up and prove the device is really pulling from it. Three failures
/// after a successful `open_stream` each end in silence behind a healthy-looking log:
///
/// 1. **The grant differs from the request.** [`grant_matches`] refuses it: another layout or
///    format is an out-of-bounds write on a realtime thread, another rate the wrong audio. The
///    NDK honours an explicit request or fails the open; this check does not trust the HAL with
///    a buffer length.
/// 2. **`request_start` fails.** The caller tries the next rung. A track plays from its open.
/// 3. **The stream starts and never calls back.** `prove_pulling` catches it, giving up once
///    that flag is set (a closing session); the last-resort reopen in [`open_any`] passes
///    `None`, since an unproven stream beats no stream. A track's writes count as callbacks.
fn arm(
    live: &LiveStream,
    fmt: PlaneFormat,
    counters: &Counters,
    prove_pulling: Option<&AtomicBool>,
) -> Result<(), ArmError> {
    grant_matches(
        live.output.granted(),
        live.rung.rate,
        fmt.rate_hz,
        fmt.channels,
    )
    .map_err(ArmError::Unusable)?;
    if let Output::AAudio(s) = &live.output {
        s.request_start()
            .map_err(|e| ArmError::Unusable(format!("request_start: {e}")))?;
        // Lift the AAudio HW buffer off its brittle ~2-burst LowLatency default so a single late
        // callback doesn't immediately underrun; the in-callback XRun loop grows it further if
        // the device still glitches. set_buffer_size_in_frames clamps to capacity.
        let burst = s.frames_per_burst().max(1);
        let _ = s.set_buffer_size_in_frames((burst * 3).min(s.buffer_capacity_in_frames()));
    }
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
/// open it again if AAudio disconnects or a track's write fails.
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
    let (mut fmt, mut first) = (fmt, None);
    while let Some(next) = supervise_format(&client, game_audio, is_tv, fmt, shutdown, first.take())
    {
        fmt = next
            .new_format
            .expect("a format ends only on a marked packet");
        log::info!("audio: the host changed the audio format — reopening the device for it");
        first = Some(next);
    }
}

/// One audio format's device ladder and its reopen-on-disconnect generations. `first` is the
/// packet that opened this format, fed before anything is pulled. `Some` = a packet in another
/// format arrived, for the next format's device.
fn supervise_format(
    client: &Arc<NativeClient>,
    game_audio: bool,
    is_tv: bool,
    fmt: PlaneFormat,
    shutdown: &AtomicBool,
    first: Option<punktfunk_core::client::AudioPacket>,
) -> Option<punktfunk_core::client::AudioPacket> {
    let mut carry = first.map(|pkt| punktfunk_core::client::AudioPacket {
        new_format: None,
        ..pkt
    });
    let playout = AudioPlayout {
        tuning: punktfunk_core::audio::JitterTuning::AAUDIO,
        format: fmt,
    };
    let counters = Arc::new(Counters::default());
    // The A/V sync hand-off: the realtime callback owns the ring (so it publishes the depth and
    // consumes the target), the decode thread owns the timestamps (so it computes the target).
    // Two atomics, because the callback must not block on the thread that decodes.
    let sync: Arc<punktfunk_core::audio::AudioSyncCell> = Arc::default();
    // API 29–31 open surround as channel indices with no mask to fix it, so an `AudioTrack`
    // plays it. API 28 still derives the speakers from the count.
    let no_mask =
        fmt.channels > 2 && crate::sys::sdk_int() >= 29 && set_channel_mask_fn().is_none();
    let track = no_mask && crate::audio_track::available();
    if no_mask && !track {
        log::warn!("audio: no Java VM for an AudioTrack — surround opens as channel indices");
    }
    // Either signal counts. Kotlin's `FEATURE_LEANBACK` is the authoritative one; the sysprop
    // catches a device reached through some path that did not pass the flag, and neither answering
    // simply keeps the phone ladder.
    let ladder = ladder_for(
        is_tv || is_tv_device(),
        fmt.rate_hz,
        track,
        sysprop(c"debug.punktfunk.audio_sharing").as_deref(),
        sysprop(c"debug.punktfunk.audio_perf").as_deref(),
    );
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

    let mut sup = Supervisor::default();
    while !shutdown.load(Ordering::Relaxed) {
        let disconnected = Arc::new(AtomicBool::new(false));
        let ctx = OpenCtx {
            fmt,
            playout,
            // Worst transient the ring can hold before the policy trims it. Through the format's
            // own conversion rather than a samples-per-millisecond constant: the cap is a hard
            // ceiling on latency, and one computed 2.3 % shallow on a 44.1-family session would
            // trim a ring that was inside its budget.
            hard_cap_max: fmt.ms_samples(playout.tuning.hard_cap_ms),
            game_audio,
            counters: &counters,
            sync: &sync,
            disconnected: &disconnected,
            shutdown,
        };
        let live = match open_any(&ladder, &ctx) {
            Some(live) => {
                sup.opened();
                live
            }
            None => match sup.no_device(shutdown.load(Ordering::Relaxed)) {
                NoDevice::Quit => return None,
                NoDevice::Retry(reopen_attempt) => {
                    log::warn!(
                        "audio: reopen attempt {reopen_attempt}/{REOPEN_ATTEMPTS} found no usable configuration — retrying"
                    );
                    nap(shutdown, REOPEN_SETTLE_MS);
                    continue;
                }
                NoDevice::GiveUp => {
                    log_no_configuration(fmt);
                    return None;
                }
            },
        };
        let generation = sup.generation;
        if generation > 0 {
            log::info!("audio: reopened after disconnect (generation {generation})");
        }
        let exit = decode_loop(
            client,
            &live,
            shutdown,
            &disconnected,
            &counters,
            playout,
            &sync,
            &mut carry,
        );
        live.output.stop();
        drop(live); // closes the stream, or stops and releases the track
        match sup.next(exit, reopen_allowed, shutdown.load(Ordering::Relaxed)) {
            Next::Reopen => {
                log::warn!("audio: device disconnected — reopening in {REOPEN_SETTLE_MS} ms");
                nap(shutdown, REOPEN_SETTLE_MS);
            }
            Next::Stop => {
                log::warn!("audio: device disconnected — not reopening (shutting down, or pinned by debug.punktfunk.audio_reopen)");
                break;
            }
            Next::End => break,
        }
    }
    log::info!(
        "audio: stopped ({}={} pcm_frames={} underruns={} generations={})",
        plane_counter_key(fmt),
        counters.frames_decoded.load(Ordering::Relaxed),
        counters.pcm_written.load(Ordering::Relaxed),
        counters.underruns.load(Ordering::Relaxed),
        sup.generation + 1,
    );
    carry.filter(|pkt| pkt.new_format.is_some())
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
            playout,
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
        // A fresh policy per open: a device that came back may burst differently.
        let policy = playout.jitter();
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

    /// One `AudioTrack` write's worth: the callback's proof of life, then [`Self::fill`].
    fn pull(&mut self, out: &mut [f32]) {
        self.counters.callbacks.fetch_add(1, Ordering::Relaxed);
        let frames = out.len() / self.channels.max(1);
        self.fill(out, i32::try_from(frames).unwrap_or(i32::MAX));
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
/// A `track` rung feeds the same ring to an `AudioTrack` writer instead of an AAudio callback.
fn try_open(rung: OpenRung, ctx: &OpenCtx) -> Result<LiveStream, String> {
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
    if rung.track {
        let cfg = crate::audio_track::TrackConfig {
            rate_hz: fmt.rate_hz,
            channels,
            low_latency: rung.perf == Perf::LowLatency,
            game_audio,
        };
        let fill = Box::new(move |out: &mut [f32]| render.pull(out));
        let track = crate::audio_track::Track::open(&cfg, ctx.disconnected.clone(), fill)?;
        return Ok(LiveStream {
            output: Output::Track(track),
            tx,
            free_rx,
            rung,
        });
    }
    let callback = move |s: &AudioStream, data: *mut c_void, num_frames: i32| {
        render.callback(s, data, num_frames)
    };

    let builder = AudioStreamBuilder::new()
        .map_err(|e| e.to_string())?
        .direction(AudioDirection::Output);
    // The wire order (FL FR FC LFE RL RR SL SR) is AAudio's speaker-bit order, so with the mask
    // `with_layout` sets this is an IDENTITY mapping — no permute.
    let builder = with_layout(builder, channels)
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
        .performance_mode(match rung.perf {
            Perf::LowLatency => AudioPerformanceMode::LowLatency,
            Perf::None => AudioPerformanceMode::None,
        })
        .sharing_mode(match rung.sharing {
            Sharing::Exclusive => AudioSharingMode::Exclusive,
            Sharing::Shared => AudioSharingMode::Shared,
        })
        .data_callback(Box::new(callback))
        .error_callback(Box::new(move |_s, e| {
            log::warn!("audio: AAudio error (device reroute/disconnect?): {e:?}");
            cb_disconnected.store(true, Ordering::SeqCst);
        }))
        .open_stream()
        .map_err(|e| e.to_string())?;
    Ok(LiveStream {
        output: Output::AAudio(stream),
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
#[allow(clippy::too_many_arguments)]
fn decode_loop(
    client: &Arc<NativeClient>,
    live: &LiveStream,
    shutdown: &AtomicBool,
    disconnected: &AtomicBool,
    counters: &Counters,
    playout: AudioPlayout,
    sync: &punktfunk_core::audio::AudioSyncCell,
    carry: &mut Option<punktfunk_core::client::AudioPacket>,
) -> DecodeExit {
    let fmt = playout.format;
    let mut plane = match Plane::new(client, live, counters, playout, sync) {
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
        let pulled = match carry.take() {
            Some(pkt) => Ok(pkt),
            None => client.next_audio(tick),
        };
        let step = match pulled {
            Ok(pkt) if pkt.new_format.is_some() => {
                *carry = Some(pkt);
                return DecodeExit::Reformat;
            }
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
        playout: AudioPlayout,
        sync: &'a punktfunk_core::audio::AudioSyncCell,
    ) -> Result<Plane<'a>, DecodeExit> {
        let fmt = playout.format;
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
        // A/V sync: this thread is the only place holding all three ingredients at once — the
        // packet's host capture `pts_ns`, the ring depth (via the sync cell) and the video plane's
        // end-to-end figure.
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
            gaps: playout.gaps(),
            drought: playout.drought(),
            last_packet: std::time::Instant::now(),
            av_sync_enabled,
            av: playout.sync(),
            video_e2e: client.video_e2e_shared(),
            av_offset_out: client.audio_av_offset_shared(),
            buffer_ms_out: client.audio_buffer_ms_shared(),
            dead_stream_warn_packets: (DEAD_STREAM_WARN_MS * 1000 / fmt.frame_us.max(1) as u64)
                .max(1),
        })
    }

    /// Hand `pcm[..n]` to the ring in a recycled buffer (allocating only when the free-list is
    /// momentarily empty: startup / after a backpressure drop). Full = drop-newest; closed ends
    /// the loop, see [`sent`]. A muted stream queues the same length in silence, so the ring
    /// keeps its cadence and the device is never closed and reopened on a toggle.
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
        sent(self.live.tx.try_send(buf))
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

    /// Device output latency past the ring, ns — see [`Output::latency_ns`].
    fn output_latency_ns(&self) -> u64 {
        self.live.output.latency_ns(self.fmt.rate_hz)
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
