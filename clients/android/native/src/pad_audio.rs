//! Pad audio on Android (the 0xD1 plane) — tier A, WP9.
//!
//! The Android twin of [`pf_client_core::pad_audio`]: drain the host's per-pad DualSense streams,
//! Opus-decode haptics (kind 0) and speaker (kind 1), interleave them into the pad's own
//! 4-channel layout, and render them on the physical pad.
//!
//! # Why this needs a USB driver instead of an audio API
//!
//! Every other client hands the 4-channel stream to the platform's audio graph — WASAPI on
//! Windows, PipeWire on Linux, CoreAudio on Apple. **Android has no such option for this device.**
//! AOSP's `UsbAlsaManager` carries a hardcoded VID/PID denylist that includes the DualSense
//! (`054c:0ce6`), so the kernel enumerates the pad's playback node and the framework then discards
//! it: `hasOutput: false`. There is no `AudioDeviceInfo` for `setPreferredDevice` to target, and
//! `/dev/snd` is closed to apps by SELinux. Android's own `UsbRequest` API cannot help either — it
//! rejects any endpoint that is not bulk or interrupt.
//!
//! So this path drives the pad's isochronous endpoint directly, through `uac-host` on the file
//! descriptor Java already owns. That is measured, not hoped: on a Nothing Phone (3) the claim
//! succeeds unprivileged, the gamepad and the pad's microphone both keep working, and the
//! underrun-free floor is **4 ms in flight** — including under eight-core load with the SoC in
//! severe thermal throttling.
//!
//! # The exclusivity that shapes everything here, and what is actually known about it
//!
//! `valid_flag0` bit 1 (`HAPTICS_SELECT`) disables the audio-haptics path, and every rumble write
//! that exists — `hid-playstation`, SDL, and our own [`crate::feedback`] path via `DsDevice` —
//! asserts it. So haptics and classic rumble cannot both drive the coils **as coded**, and the
//! arbitration selects rather than blends.
//!
//! What is NOT established is the stronger claim this module used to make: that the coils and the
//! rumble motors are the same physical actuators, exclusive *in the firmware*. No teardown, vendor
//! document or measurement supports it here; it traces to one reverse-engineered comment in SDL,
//! and SDL's own modern path sets `HAPTICS_SELECT` **alone** (amplitude rides `ucEnableBits3`),
//! which reads more like an independent mute for the audio path than a shared-actuator interlock.
//! The combination that would settle it — rumble asserted with `HAPTICS_SELECT` CLEARED — is
//! emitted by no code anywhere, so nothing here writes it either.
//!
//! The arbitration is therefore built on **evidence, not prediction**: haptics owns the coils only
//! while haptics frames are actually arriving (see [`haptics_owns_coils`]). That is correct under
//! either hypothesis, and it is what keeps a rumble-only title rumbling — it renders no haptics
//! audio, the host's silence gate emits nothing, and the pad simply keeps its motors.

use punktfunk_core::audio::pad_mix::HapticsLiveness;
#[cfg(target_os = "android")]
use punktfunk_core::audio::pad_mix::{
    is_haptics_evidence, PadDecode, QuadMixer, MAX_FRAME_SAMPLES, PAD_CHANNELS,
};

#[cfg(target_os = "android")]
use punktfunk_core::client::NativeClient;
#[cfg(target_os = "android")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "android")]
use std::sync::Arc;
#[cfg(target_os = "android")]
use std::thread::JoinHandle;
#[cfg(target_os = "android")]
use std::time::{Duration, Instant};

/// Both plane kinds decode as 48 kHz stereo.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const SAMPLE_RATE: u32 = 48_000;

/// Ring ceiling, in sample frames. 60 ms — far above the in-flight depth, because this bounds
/// *decoder* backlog when the USB side stalls, not stream latency. Overflow drops the oldest.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const MAX_BUFFER_FRAMES: usize = (SAMPLE_RATE as usize / 1000) * 60;

/// How much audio to keep in flight on the USB endpoint.
///
/// WP7 measured the underrun-free floor on real hardware at **4 ms** (clean across three sweeps,
/// including one under eight-core load with the CPU thermally throttled); 3 ms was marginal and
/// 2 ms never survived. 6 ms takes one step of headroom above that floor, because the same
/// measurement found isolated transient events roughly once per three seconds that are *not*
/// depth-dependent — so the floor is a floor, not a target.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const IN_FLIGHT_MS: u32 = 6;

// ---- tier-A registry ---------------------------------------------------------------------------

/// Which wire pad indices are currently rendering tier-A audio, as a bitmask over the 16 wire
/// slots.
///
/// Read on the rumble poll thread and written on the JNI thread, so it is an atomic rather than a
/// lock: the reader is on a latency path and must never block behind a start/stop.
static TIER_A_PADS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Mark (or clear) a pad as rendering tier-A audio.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn set_tier_a(pad: u8, on: bool) {
    use std::sync::atomic::Ordering;
    let bit = 1u32 << (pad & 0x0f);
    if on {
        TIER_A_PADS.fetch_or(bit, Ordering::Relaxed);
    } else {
        TIER_A_PADS.fetch_and(!bit, Ordering::Relaxed);
    }
}

/// Is this pad's HAPTICS lane armed — i.e. did a renderer open a stream that wants the coils?
///
/// Armed is necessary but NOT sufficient to take the pad off wire rumble; see
/// [`haptics_owns_coils`]. Speaker-only rendering never arms this: the speaker pair is a
/// different pair of channels and cannot be disturbed by a rumble write.
pub(crate) fn haptics_armed(pad: u8) -> bool {
    TIER_A_PADS.load(std::sync::atomic::Ordering::Relaxed) & (1u32 << (pad & 0x0f)) != 0
}

/// Last real (non-concealed) haptics frame per pad. Written by the render thread, read by the
/// rumble poll thread.
static HAPTICS: HapticsLiveness = HapticsLiveness::new();

/// Clear a pad's liveness (slot teardown). Wire indices are recycled, so a stale stamp would let
/// a fresh pad inherit the previous one's ownership.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn clear_haptics_liveness(pad: u8) {
    HAPTICS.clear(pad);
}

/// Does this pad's haptics stream currently own the coils, so wire rumble must stand down?
///
/// Arbitrating on **evidence** rather than on a prediction about the hardware is deliberate. Every
/// rumble write this tree emits asserts `valid_flag0` bit 1 (`HAPTICS_SELECT`), which disables the
/// audio-haptics path, so the two cannot both drive the coils *as coded* — whatever the firmware
/// would allow. But a title that never renders haptics audio produces no 0xD1 frames at all, and
/// suppressing its rumble on the assumption that "the stream carries the feedback" silences it
/// outright. Frame arrival is the signal that tells the two cases apart, and it costs nothing.
pub(crate) fn haptics_owns_coils(pad: u8) -> bool {
    haptics_armed(pad) && HAPTICS.live(pad)
}

// ---- the USB sink ------------------------------------------------------------------------------

/// Everything that talks to the pad. Linux and Android only: `usbfs` is a Linux kernel ABI, and
/// this crate also builds as a host cdylib on macOS dev boxes, where the registry above still
/// compiles and runs its tests.
#[cfg(target_os = "android")]
mod sink {
    use super::{IN_FLIGHT_MS, PAD_CHANNELS, SAMPLE_RATE};

    /// Open the pad's 4-channel playback stream on a descriptor Java owns.
    ///
    /// # Safety
    ///
    /// `fd` must be a live usbfs descriptor from an open `UsbDeviceConnection` that outlives the
    /// returned device — this **borrows** it and never closes it, because closing is
    /// `UsbDeviceConnection.close()`'s job and a double close would strand an unrelated
    /// descriptor much later.
    pub(super) unsafe fn device(fd: i32) -> usbfs_iso::UsbFsDevice {
        // SAFETY: forwarded from this function's own contract, which the JNI entry point upholds
        // by keeping the Java connection open for the lifetime of the renderer thread.
        unsafe { usbfs_iso::UsbFsDevice::from_borrowed_fd(fd) }
    }

    /// Find the pad's 4-channel playback stream and open it.
    ///
    /// Four channels is a hard requirement, not a preference: the voice coils *are* channels 3
    /// and 4, so a 2-channel alternate setting would open successfully and then render haptics
    /// into nothing.
    pub(super) fn open<'d>(
        dev: &'d usbfs_iso::UsbFsDevice,
    ) -> Result<uac_host::Playback<'d>, uac_host::Error> {
        let blob = dev.raw_descriptors()?;
        let function = uac_host::parse(&blob)?;
        let stream = function
            .output_streams()
            .find(|s| usize::from(s.channels()) == PAD_CHANNELS)
            .ok_or(uac_host::Error::NoAudioFunction)?;

        let opts = uac_host::OpenOptions {
            depth: usbfs_iso::Depth::Millis(IN_FLIGHT_MS),
            // One packet per URB: the finest granularity the bus offers, and what WP7 measured
            // the 4 ms floor with. Packing more multiplies one completion's latency.
            packets_per_urb: Some(1),
            // Keep the endpoint fed rather than gapping when the decoder is momentarily late.
            // A hole in an isochronous stream is silence forever; silence we chose is better.
            underrun: usbfs_iso::Underrun::FillSilence,
            ..Default::default()
        };
        stream.open_with(dev, uac_host::Format::S16Le, SAMPLE_RATE, opts)
    }
}

// ---- the self test ------------------------------------------------------------------------------

/// Drive the pad directly with a synthetic tone, through **the real client path**.
///
/// This exists because the two things most likely to be wrong here cannot be unit-tested and are
/// invisible without a host: whether the descriptor Kotlin handed over is one this renderer may
/// drive exclusively, and whether the interface claim succeeds on this kernel. A standalone
/// harness proves neither — it owns its descriptor by construction, which is exactly the condition
/// that was violated when this renderer was handed the HID link's fd and the two engines began
/// stealing each other's URB completions.
///
/// Opens the sink the same way [`render`] does and writes a sine into the voice-coil pair, which
/// is felt rather than heard. Returns sample frames written, or a negative [`SelfTest`] code.
///
/// # Safety
///
/// `fd` must be a live usbfs descriptor whose connection outlives the call, and which **nothing
/// else is driving transfers on**.
#[cfg(target_os = "android")]
pub(crate) unsafe fn self_test(fd: i32, seconds: i32, hz: i32) -> i32 {
    // SAFETY: the caller's contract.
    let dev = unsafe { sink::device(fd) };
    let mut playback = match sink::open(&dev) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("pad audio self-test: open failed: {e}");
            return SelfTest::OPEN_FAILED;
        }
    };
    log::info!(
        "pad audio self-test: {} ch {} at {} Hz, {} us in flight",
        playback.channels(),
        playback.format(),
        playback.rate(),
        playback.schedule().in_flight_us()
    );

    let rate = playback.rate();
    let channels = playback.channels() as usize;
    let frames_per_chunk = (rate as usize / 1000).max(1);
    let mut chunk = vec![0i16; frames_per_chunk * channels];
    let mut phase = 0.0f32;
    let step = std::f32::consts::TAU * hz.clamp(20, 500) as f32 / rate as f32;
    let total = u64::from(rate) * seconds.clamp(1, 30) as u64;
    let mut written = 0u64;

    while written < total {
        for frame in chunk.chunks_mut(channels) {
            let sample = (phase.sin() * 16_384.0) as i16;
            phase += step;
            if phase >= std::f32::consts::TAU {
                phase -= std::f32::consts::TAU;
            }
            frame.fill(0);
            // Channels 2 and 3 are the voice coils; the speaker pair stays silent so a pass is
            // unambiguously FELT rather than merely audible.
            for slot in frame.iter_mut().take(channels).skip(2) {
                *slot = sample;
            }
        }
        if let Err(e) = playback.write_interleaved(&chunk) {
            log::warn!("pad audio self-test: write failed after {written} frames: {e}");
            return SelfTest::WRITE_FAILED;
        }
        written += frames_per_chunk as u64;
    }
    let _ = playback.drain(Duration::from_millis(500));

    let stats = playback.stats();
    log::info!(
        "pad audio self-test: {} frames, {} urbs, {} underruns, {} short bytes, {} urb errors",
        playback.frames_written(),
        stats.urbs_completed,
        stats.underruns,
        stats.short_bytes,
        stats.urb_errors
    );
    // Underruns are a producer-pacing property and deliberately NOT a failure here: the question
    // this answers is whether the client can drive the pad at all. Data reaching the bus is the
    // pass condition.
    if stats.urb_errors > 0 || playback.frames_written() == 0 {
        return SelfTest::NO_DATA;
    }
    playback.frames_written().min(i32::MAX as u64) as i32
}

/// Negative results from [`self_test`]. Positive values are sample frames written.
#[cfg(target_os = "android")]
pub(crate) struct SelfTest;

#[cfg(target_os = "android")]
impl SelfTest {
    /// The claim or stream open failed — the OEM-kernel case, or a descriptor another engine owns.
    pub(crate) const OPEN_FAILED: i32 = -1;
    /// The stream opened but a write failed part-way.
    pub(crate) const WRITE_FAILED: i32 = -2;
    /// It ran, but nothing reached the bus.
    pub(crate) const NO_DATA: i32 = -3;
}

// ---- the renderer worker -----------------------------------------------------------------------

/// A running renderer: the stop flag and the thread, joined on drop.
///
/// Mirrors [`crate::mic::MicCapture`]'s discipline — dropping the handle is what stops the stream,
/// so a session teardown that forgets a step cannot leave a thread writing to a descriptor Java is
/// about to close.
#[cfg(target_os = "android")]
pub(crate) struct PadAudio {
    pad: u8,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

#[cfg(target_os = "android")]
impl Drop for PadAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        // Belt and braces: the thread clears these itself on the way out, but if it died in a way
        // that skipped that, leaving the pad off wire rumble would cost the user all feedback.
        set_tier_a(self.pad, false);
        clear_haptics_liveness(self.pad);
    }
}

/// Start the renderer for a pad whose descriptor Java has handed over.
///
/// Returns `None` when neither kind is enabled (nothing to render) or the thread will not start.
///
/// # Safety
/// `fd` is a live usbfs descriptor whose `UsbDeviceConnection` stays open until the returned
/// handle is dropped — the renderer borrows it and never closes it.
#[cfg(target_os = "android")]
pub(crate) unsafe fn start(
    client: Arc<NativeClient>,
    pad: u8,
    fd: i32,
    haptics: bool,
    speaker: bool,
) -> Option<PadAudio> {
    if !haptics && !speaker {
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    // SAFETY: forwarded from this function's contract; dropping the handle joins the thread.
    let join = unsafe { spawn(client, Arc::clone(&stop), pad, fd, haptics, speaker) }?;
    Some(PadAudio {
        pad,
        stop,
        join: Some(join),
    })
}

/// Spawn the pad-audio renderer — the 0xD1 plane's single consumer on Android. Returns `None` if
/// the thread could not be started.
///
/// # Safety
/// `fd` is the pad's usbfs descriptor from `UsbDeviceConnection.getFileDescriptor()`, and that
/// connection stays open until the returned handle is joined.
#[cfg(target_os = "android")]
unsafe fn spawn(
    client: Arc<NativeClient>,
    stop: Arc<AtomicBool>,
    pad: u8,
    fd: i32,
    haptics: bool,
    speaker: bool,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("pf-pad-audio".into())
        // SAFETY: the caller keeps `fd` open until this thread is joined.
        .spawn(move || unsafe { run(&client, &stop, pad, fd, haptics, speaker) })
        .map_err(|e| log::warn!("pad-audio thread not started: {e}"))
        .ok()
}

/// The renderer thread's body.
///
/// # Safety
/// `fd` stays open until this returns (see [`spawn`]).
#[cfg(target_os = "android")]
unsafe fn run(
    client: &NativeClient,
    stop: &AtomicBool,
    pad: u8,
    fd: i32,
    haptics: bool,
    speaker: bool,
) {
    // Ask the scheduler for audio priority. Android does not hand SCHED_FIFO to ordinary app
    // threads, so -16 (ANDROID_PRIORITY_AUDIO) is the realistic knob — and WP7 measured that it
    // both applies and is enough to hold the 4 ms floor against eight busy cores.
    let _ = crate::sys::set_thread_nice(None, -16);

    // SAFETY: forwarded from this function's contract.
    let dev = unsafe { sink::device(fd) };
    // Through a reference, deliberately: `UsbFsDevice` has a `Drop`, and opening the stream in
    // this same scope would make the borrow outlive the value it borrows.
    render(&dev, client, stop, pad, haptics, speaker);
}

/// Open the pad's stream and render on it until the session stops or the device goes away.
#[cfg(target_os = "android")]
fn render(
    dev: &usbfs_iso::UsbFsDevice,
    client: &NativeClient,
    stop: &AtomicBool,
    pad: u8,
    haptics: bool,
    speaker: bool,
) {
    // A host that cannot send 0xD1 will never render anything here, so opening the stream would
    // claim the interface and (before the arbitration below) take the pad off wire rumble in
    // exchange for nothing. Against every released host this is the DEFAULT path — `pad_haptics`
    // is on with no UI to turn it off — so without this gate a wired DualSense simply stops
    // rumbling. Checked before `sink::open` so the iso interface is never claimed pointlessly.
    if client.host_caps() & punktfunk_core::quic::HOST_CAP_PAD_AUDIO == 0 {
        log::warn!("pad audio: host cannot send it (no HOST_CAP_PAD_AUDIO) — pad {pad} stays on wire rumble");
        drain_until_stop(client, stop);
        return;
    }
    match sink::open(dev) {
        Ok(mut playback) => {
            log::info!(
                "pad audio: pad={pad} {} ch {} at {} Hz, {} us in flight",
                playback.channels(),
                playback.format(),
                playback.rate(),
                playback.schedule().in_flight_us()
            );
            // ONLY NOW commit the trade. Declaring the pad's render capability makes the host
            // emit 0xD1, and taking the pad off wire rumble is what makes tier A and tier C
            // mutually exclusive — doing either before the stream is known to open would, on a
            // kernel that refuses the claim, leave the user with no haptics of any kind.
            let caps = (if haptics { 0x01 } else { 0 }) | (if speaker { 0x02 } else { 0 });
            client.set_pad_audio_caps(pad, caps);
            // Arm the HAPTICS lane only — a speaker-only setup drives channels 0/1, which no
            // rumble write can disturb, so taking the motors away would kill rumble with nothing
            // rendering haptics in exchange.
            set_tier_a(pad, haptics);

            pump(client, stop, pad, haptics, speaker, &mut playback);

            // Give the pad back to wire rumble before this thread goes away.
            client.set_pad_audio_caps(pad, 0);
            set_tier_a(pad, false);
            clear_haptics_liveness(pad);
        }
        Err(e) => {
            // A kernel that refuses the claim: some OEM kernels do, and there is no app-side fix.
            // Nothing was declared and nothing was suppressed, so the session simply carries on
            // at tier C with ordinary rumble — a clean degrade rather than silent total loss.
            log::warn!("pad audio unavailable on pad {pad}, staying on rumble: {e}");
            drain_until_stop(client, stop);
        }
    }
}

#[cfg(target_os = "android")]
/// Keep the plane drained without rendering, so a host that is sending 0xD1 does not back up
/// against a consumer that never reads.
fn drain_until_stop(client: &NativeClient, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        if client.next_pad_audio(Duration::from_millis(20)).is_none()
            && (stop.load(Ordering::Relaxed) || client.is_session_ended())
        {
            return;
        }
    }
}

/// The steady state: decode arriving frames, interleave, and hand whole frames to the pad.
#[cfg(target_os = "android")]
fn pump(
    client: &NativeClient,
    stop: &AtomicBool,
    pad: u8,
    haptics: bool,
    speaker: bool,
    playback: &mut uac_host::Playback<'_>,
) {
    let mut mixer = QuadMixer::<i16>::new(MAX_BUFFER_FRAMES);
    let mut stage = PadDecode::new(haptics, speaker);
    let mut tally = Tally::new();
    let mut pcm = vec![0i16; MAX_FRAME_SAMPLES * 2];
    let mut out: Vec<i16> = Vec::with_capacity(MAX_BUFFER_FRAMES * PAD_CHANNELS);

    while !stop.load(Ordering::Relaxed) {
        // Report BEFORE the frame gate. Silence on the plane is a legitimate — and highly
        // diagnostic — state: it means the host's capture hears nothing, which is a routing
        // problem upstream rather than anything here. Reporting only when a frame arrives makes
        // that state indistinguishable from the renderer being dead.
        tally.report(playback);

        let mut next = client.next_pad_audio(Duration::from_millis(10));
        if next.is_none() {
            // R12: `next_pad_audio` collapses a DISCONNECTED channel into the same `None` as an
            // ordinary timeout, so this arm cannot tell "nothing arrived in 10 ms" from "the
            // session is gone and nothing will ever arrive again". Left to `continue`, a closed
            // session span this loop at nice -16 until the owner's stop flag caught up. Ask the
            // connection directly.
            if client.is_session_ended() {
                log::debug!("pad audio: session ended, leaving the render loop");
                break;
            }
            continue;
        }
        // Everything queued goes into the mixer before one write, so its MAX_BUFFER_FRAMES
        // ceiling bounds the backlog. One frame per USB-paced write never sheds a burst.
        while let Some(frame) = next.take() {
            next = client.next_pad_audio(Duration::ZERO);

            // R14: `PadAudioFrame` carries the wire pad it was addressed to, and this renderer serves
            // exactly one. A frame for another pad — a queue still holding the previous occupant's
            // when a slot is re-used, or a host bug — would otherwise be decoded here AND seed the
            // gap tracker from a foreign sequence space, which shows up as a burst of phantom
            // concealment rather than as anything obviously wrong.
            if frame.pad != pad {
                log::debug!(
                    "pad audio: dropping frame for pad {} on pad {pad}",
                    frame.pad
                );
                continue;
            }

            // Haptics off with the speaker on is a legitimate setup, and the host may send both.
            if !stage.wants(frame.kind) {
                continue;
            }

            // Stamped on arrival, so a decoder hiccup cannot hand the coils back mid-effect. The
            // sink is open for the whole pump.
            if is_haptics_evidence(&frame, true) {
                HAPTICS.note(pad);
            }

            tally.frames_in += 1;
            if let Some(n) = stage.decode_frame(&frame, &mut pcm, &mut mixer) {
                tally.decoded(&pcm[..n * 2]);
            }
        }

        // Hand over whole frames only. `write` stages any remainder internally, so a partial
        // chunk is never padded with silence mid-stream.
        out.clear();
        if mixer.pop(&mut out, Instant::now()) > 0
            && !write_out(playback, &out, &mut mixer, &mut tally)
        {
            return;
        }
    }

    let _ = playback.drain(Duration::from_millis(100));
    let stats = playback.stats();
    log::info!(
        "pad audio stopped: {} frames, {} underruns, {} short bytes, {} dropped by backlog",
        playback.frames_written(),
        stats.underruns,
        stats.short_bytes,
        mixer.dropped_frames(),
    );
}

/// One write to the endpoint. `false` = the stream is gone. A SHORT write is back-pressure, not
/// success: the tail cannot be retried without unbounded buffering (the mixer's whole point is to
/// stay ahead of the device), so it is dropped but COUNTED — the difference between a
/// diagnosable stall and a mystery.
#[cfg(target_os = "android")]
fn write_out(
    playback: &mut uac_host::Playback<'_>,
    out: &[i16],
    mixer: &mut QuadMixer<i16>,
    tally: &mut Tally,
) -> bool {
    match playback.write_interleaved(out) {
        Ok(n) if n < out.len() => tally.short_write(n, out.len()),
        Ok(_) => {}
        Err(e) => {
            if is_fatal(&e) {
                log::warn!("pad audio: stream lost: {e}");
                return false;
            }
            log::debug!("pad audio: write hiccup: {e}");
            mixer.discard();
        }
    }
    true
}

/// Periodic accounting. Without it the only way to tell "the host is sending nothing" from
/// "frames arrive but render silently" is to guess, and those two have completely different
/// causes — one is host-side routing, the other is here.
#[cfg(target_os = "android")]
struct Tally {
    frames_in: u64,
    samples_in: u64,
    peak: i32,
    last_report: std::time::Instant,
    /// R13: caller-side short-write accounting (distinct from `st.short_bytes`, a URB-level
    /// statistic from inside the transport).
    st_short: u64,
    st_short_logged: std::time::Instant,
}

#[cfg(target_os = "android")]
impl Tally {
    fn new() -> Tally {
        let now = std::time::Instant::now();
        Tally {
            frames_in: 0,
            samples_in: 0,
            peak: 0,
            last_report: now,
            st_short: 0,
            st_short_logged: now,
        }
    }

    /// The 1 s line.
    fn report(&mut self, playback: &uac_host::Playback<'_>) {
        if self.last_report.elapsed() < Duration::from_secs(1) {
            return;
        }
        let st = playback.stats();
        log::info!(
            "pad audio: {} frames in, {} samples, peak={}, {} written, {} underruns, {} short, \
             {} dropped to back-pressure",
            self.frames_in,
            self.samples_in,
            self.peak,
            playback.frames_written(),
            st.underruns,
            st.short_bytes,
            self.st_short,
        );
        self.last_report = std::time::Instant::now();
        self.peak = 0;
    }

    /// Count one decoded frame and its peak. The peak tells "frames arriving but silent" (host
    /// routing) from "signal that does not reach the actuators" (here).
    fn decoded(&mut self, stereo: &[i16]) {
        self.samples_in += (stereo.len() / 2) as u64;
        let peak = stereo.iter().map(|s| i32::from(s.abs())).max();
        self.peak = self.peak.max(peak.unwrap_or(0));
    }

    fn short_write(&mut self, wrote: usize, len: usize) {
        self.st_short += (len - wrote) as u64;
        if self.st_short_logged.elapsed() >= Duration::from_secs(5) {
            log::warn!(
                "pad audio: endpoint short-wrote {wrote} of {len} samples ({} total) — the device \
                 is not keeping up",
                self.st_short
            );
            self.st_short_logged = std::time::Instant::now();
        }
    }
}

/// Is this the end of the stream, or just a bad moment?
///
/// A vanished device is unrecoverable here — the descriptor belongs to a `UsbDeviceConnection`
/// that Java must re-open — so the thread exits and the session continues without tier A. Anything
/// else is treated as transient.
#[cfg(target_os = "android")]
fn is_fatal(e: &uac_host::Error) -> bool {
    matches!(e, uac_host::Error::Transport(t) if t.is_disconnected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_a_registry_tracks_pads_independently() {
        // A rumble command reaching a tier-A pad mutes its coils for the session, so this gate
        // has to be exact rather than approximately right.
        set_tier_a(3, true);
        assert!(haptics_armed(3));
        assert!(!haptics_armed(4));
        set_tier_a(4, true);
        assert!(haptics_armed(3) && haptics_armed(4));
        set_tier_a(3, false);
        assert!(!haptics_armed(3), "clearing one pad must not clear another");
        assert!(haptics_armed(4));
        set_tier_a(4, false);
        assert!(!haptics_armed(4));
    }

    #[test]
    fn tier_a_registry_wraps_the_pad_index_into_the_wire_slot_space() {
        // The wire pad space is 4 bits; an out-of-range index must not shift the mask into
        // undefined territory (a shift >= 32 is a panic in debug and garbage in release).
        set_tier_a(0x1f, true);
        assert!(haptics_armed(0x0f), "0x1f and 0x0f are the same wire slot");
        set_tier_a(0x0f, false);
        assert!(!haptics_armed(0x1f));
    }

    /// Armed alone is not ownership, and teardown drops the stamp: wire indices are recycled,
    /// and a fresh pad must not inherit the previous occupant's coils.
    #[test]
    fn haptics_own_the_coils_only_while_armed_and_live() {
        HAPTICS.note(2);
        assert!(
            !haptics_owns_coils(2),
            "frames without a renderer own nothing"
        );
        set_tier_a(2, true);
        assert!(haptics_owns_coils(2));
        clear_haptics_liveness(2);
        assert!(
            !haptics_owns_coils(2),
            "a cleared stamp must release the coils"
        );
        set_tier_a(2, false);
    }
}
