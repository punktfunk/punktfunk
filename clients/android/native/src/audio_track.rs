//! Surround playback through a Java `AudioTrack`, for API 29–31.
//!
//! There AAudio opens more than two channels as an index mask and has no channel mask to say
//! otherwise, so a stereo mixer keeps FL/FR. `AudioTrack` takes a positional mask on every API
//! level, so 5.1 and 7.1 reach the speakers, or AudioFlinger's downmix. It has no pull callback: a
//! writer thread fills each chunk from the same jitter ring [`crate::audio`] drains, and blocks in
//! `write` until the track takes it.

use crate::audio_policy::{grown, heard_at, XrunGrowth};
use jni::errors::Result as JniResult;
use jni::objects::{JObject, JValue};
use jni::refs::Global;
use jni::strings::JNIStr;
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// `AudioFormat.ENCODING_PCM_FLOAT`.
const ENCODING_PCM_FLOAT: i32 = 4;
/// `AudioTrack.MODE_STREAM`.
const MODE_STREAM: i32 = 1;
/// `AudioTrack.PERFORMANCE_MODE_LOW_LATENCY` (API 26).
const PERFORMANCE_MODE_LOW_LATENCY: i32 = 1;
/// `AudioTrack.WRITE_BLOCKING`.
const WRITE_BLOCKING: i32 = 0;
/// `AudioTrack.STATE_INITIALIZED`.
const STATE_INITIALIZED: i32 = 1;
/// `AudioAttributes.USAGE_GAME` and `CONTENT_TYPE_MOVIE`: AAudio's game-audio tagging.
const USAGE_GAME: i32 = 14;
const CONTENT_TYPE_MOVIE: i32 = 3;
/// Writes between `getTimestamp` reads, ~0.5 s. The play time in between is extrapolated.
const TIMESTAMP_EVERY: u32 = 100;

/// Fills one chunk of interleaved f32 from the jitter ring.
pub(crate) type Fill = Box<dyn FnMut(&mut [f32]) + Send>;

/// What one open asks of the track.
pub(crate) struct TrackConfig {
    pub rate_hz: u32,
    pub channels: u8,
    pub low_latency: bool,
    pub game_audio: bool,
}

/// What the track granted, read back after `build`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Granted {
    pub rate_hz: i32,
    pub channels: i32,
    pub perf: i32,
    pub buffer_frames: i32,
    pub capacity_frames: i32,
}

/// Whether native threads can reach the Java VM: true once a `NativeBridge` call ran `with_env`.
pub(crate) fn available() -> bool {
    JavaVM::singleton().is_ok()
}

/// One playing `AudioTrack` and its writer thread. Dropping it stops, joins and releases.
pub(crate) struct Track {
    vm: JavaVM,
    track: Arc<Global<JObject<'static>>>,
    stop: Arc<AtomicBool>,
    heard_at_ns: Arc<AtomicI64>,
    writer: Option<JoinHandle<()>>,
    pub granted: Granted,
}

impl Track {
    /// Build and start the track, then spawn the writer. `fill` gets one chunk of interleaved
    /// f32 per write. A failed write sets `disconnected`, which the supervisor reopens on.
    pub(crate) fn open(
        cfg: &TrackConfig,
        disconnected: Arc<AtomicBool>,
        fill: Fill,
    ) -> Result<Track, String> {
        let vm = JavaVM::singleton().map_err(|e| format!("reach the Java VM: {e}"))?;
        let (track, granted) = vm
            .attach_current_thread(|env| -> Result<_, Box<dyn std::error::Error>> {
                let track = build(env, cfg)?;
                if int(env, &track, jni_str!("getState"))? != STATE_INITIALIZED {
                    void(env, &track, jni_str!("release"))?;
                    return Err("AudioTrack did not initialize".into());
                }
                let granted = Granted {
                    rate_hz: int(env, &track, jni_str!("getSampleRate"))?,
                    channels: int(env, &track, jni_str!("getChannelCount"))?,
                    perf: int(env, &track, jni_str!("getPerformanceMode"))?,
                    buffer_frames: int(env, &track, jni_str!("getBufferSizeInFrames"))?,
                    capacity_frames: int(env, &track, jni_str!("getBufferCapacityInFrames"))?,
                };
                void(env, &track, jni_str!("play"))?;
                Ok((env.new_global_ref(&track)?, granted))
            })
            .map_err(|e| format!("open AudioTrack: {e}"))?;
        let track = Arc::new(track);
        let stop = Arc::new(AtomicBool::new(false));
        let heard_at_ns = Arc::new(AtomicI64::new(0));
        // 5 ms per write: one Opus frame, and AAudio's callback size on most low-latency paths.
        let chunk_frames = (cfg.rate_hz / 200).max(1) as usize;
        let mut writer = Writer {
            fill,
            buf: vec![0.0; chunk_frames * usize::from(cfg.channels)],
            channels: i64::from(cfg.channels),
            rate_hz: cfg.rate_hz,
            chunk_frames: chunk_frames as i32,
            stop: stop.clone(),
            heard_at_ns: heard_at_ns.clone(),
        };
        let (wvm, wtrack) = (vm.clone(), track.clone());
        let spawned = std::thread::Builder::new()
            .name("pf-audio-track".into())
            .spawn(move || {
                crate::audio::boost_audio_thread("audio-track");
                let ran = wvm.attach_current_thread(|env| writer.run(env, &wtrack));
                let failed = match ran {
                    Ok(code) => code
                        .filter(|_| !writer.stop.load(Ordering::Relaxed))
                        .map(|c| format!("write returned {c}")),
                    Err(e) => (!writer.stop.load(Ordering::Relaxed)).then(|| e.to_string()),
                };
                if let Some(why) = failed {
                    log::warn!("audio: AudioTrack writer stopped ({why}) — reopening");
                    disconnected.store(true, Ordering::SeqCst);
                }
            });
        let mut opened = Track {
            vm,
            track,
            stop,
            heard_at_ns,
            writer: None,
            granted,
        };
        opened.writer = Some(spawned.map_err(|e| format!("spawn the AudioTrack writer: {e}"))?);
        Ok(opened)
    }

    /// When the newest written frame plays, `CLOCK_MONOTONIC` ns; 0 before the first timestamp.
    pub(crate) fn heard_at_ns(&self) -> i64 {
        self.heard_at_ns.load(Ordering::Relaxed)
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // `stop()` interrupts a blocking `write`, and writes after it return at once.
        let _ = self
            .vm
            .attach_current_thread(|env| void(env, &self.track, jni_str!("stop")));
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        let _ = self
            .vm
            .attach_current_thread(|env| void(env, &self.track, jni_str!("release")));
    }
}

/// The writer thread's state: the ring's fill, one chunk, and what it publishes.
struct Writer {
    fill: Fill,
    buf: Vec<f32>,
    channels: i64,
    rate_hz: u32,
    chunk_frames: i32,
    stop: Arc<AtomicBool>,
    heard_at_ns: Arc<AtomicI64>,
}

impl Writer {
    /// Fill and write chunks until stopped. `Some(code)` is a negative `write` result.
    fn run(&mut self, env: &mut Env, track: &JObject) -> JniResult<Option<i32>> {
        let len = i32::try_from(self.buf.len()).unwrap_or(i32::MAX);
        let arr = env.new_float_array(self.buf.len())?;
        let ts = env.new_object(
            jni_str!("android/media/AudioTimestamp"),
            jni_sig!("()V"),
            &[],
        )?;
        let (mut written, mut anchor, mut n) = (0i64, None, 0u32);
        let mut growth = XrunGrowth::default();
        while !self.stop.load(Ordering::Relaxed) {
            (self.fill)(&mut self.buf);
            arr.set_region(env, 0, &self.buf)?;
            let wrote = env
                .call_method(
                    track,
                    jni_str!("write"),
                    jni_sig!("([FIII)I"),
                    &[
                        JValue::Object(&arr),
                        JValue::Int(0),
                        JValue::Int(len),
                        JValue::Int(WRITE_BLOCKING),
                    ],
                )?
                .i()?;
            if wrote < 0 {
                return Ok(Some(wrote));
            }
            written += i64::from(wrote) / self.channels;
            n = n.wrapping_add(1);
            if anchor.is_none() || n % TIMESTAMP_EVERY == 0 {
                anchor = timestamp(env, track, &ts)?.or(anchor);
            }
            if let Some((pos, at)) = anchor {
                let heard = heard_at(at, pos, written, self.rate_hz);
                self.heard_at_ns.store(heard, Ordering::Relaxed);
            }
            if growth.due() {
                self.grow_on_underrun(env, track, &mut growth)?;
            }
        }
        Ok(None)
    }

    /// AAudio's anti-glitch step, by [`XrunGrowth`]: new underruns grow the buffer by one chunk,
    /// up to capacity.
    fn grow_on_underrun(
        &self,
        env: &mut Env,
        track: &JObject,
        growth: &mut XrunGrowth,
    ) -> JniResult<()> {
        if !growth.rose(int(env, track, jni_str!("getUnderrunCount"))?) {
            return Ok(());
        }
        let size = int(env, track, jni_str!("getBufferSizeInFrames"))?;
        let cap = int(env, track, jni_str!("getBufferCapacityInFrames"))?;
        env.call_method(
            track,
            jni_str!("setBufferSizeInFrames"),
            jni_sig!("(I)I"),
            &[JValue::Int(grown(size, self.chunk_frames, cap))],
        )?;
        Ok(())
    }
}

/// `(framePosition, nanoTime)` of the track's latest timestamp, or `None` before playback starts.
fn timestamp(env: &mut Env, track: &JObject, ts: &JObject) -> JniResult<Option<(i64, i64)>> {
    let got = env
        .call_method(
            track,
            jni_str!("getTimestamp"),
            jni_sig!("(Landroid/media/AudioTimestamp;)Z"),
            &[JValue::Object(ts)],
        )?
        .z()?;
    if !got {
        return Ok(None);
    }
    let pos = env
        .get_field(ts, jni_str!("framePosition"), jni_sig!("J"))?
        .j()?;
    let at = env
        .get_field(ts, jni_str!("nanoTime"), jni_sig!("J"))?
        .j()?;
    Ok(Some((pos, at)))
}

/// `new AudioTrack.Builder()…build()` for `cfg`: float PCM, the wire's speaker mask, stream mode.
fn build<'local>(env: &mut Env<'local>, cfg: &TrackConfig) -> JniResult<JObject<'local>> {
    let mask = crate::audio_format::java_channel_mask(cfg.channels);
    let fb = env.new_object(
        jni_str!("android/media/AudioFormat$Builder"),
        jni_sig!("()V"),
        &[],
    )?;
    for (setter, value) in [
        (jni_str!("setEncoding"), ENCODING_PCM_FLOAT),
        (jni_str!("setSampleRate"), cfg.rate_hz as i32),
        (jni_str!("setChannelMask"), mask),
    ] {
        env.call_method(
            &fb,
            setter,
            jni_sig!("(I)Landroid/media/AudioFormat$Builder;"),
            &[JValue::Int(value)],
        )?;
    }
    let format = env
        .call_method(
            &fb,
            jni_str!("build"),
            jni_sig!("()Landroid/media/AudioFormat;"),
            &[],
        )?
        .l()?;
    let tb = env.new_object(
        jni_str!("android/media/AudioTrack$Builder"),
        jni_sig!("()V"),
        &[],
    )?;
    env.call_method(
        &tb,
        jni_str!("setAudioFormat"),
        jni_sig!("(Landroid/media/AudioFormat;)Landroid/media/AudioTrack$Builder;"),
        &[JValue::Object(&format)],
    )?;
    let mut ints = vec![(jni_str!("setTransferMode"), MODE_STREAM)];
    if cfg.low_latency {
        ints.push((jni_str!("setPerformanceMode"), PERFORMANCE_MODE_LOW_LATENCY));
    }
    for (setter, value) in ints {
        env.call_method(
            &tb,
            setter,
            jni_sig!("(I)Landroid/media/AudioTrack$Builder;"),
            &[JValue::Int(value)],
        )?;
    }
    // Untagged, the builder's default is USAGE_MEDIA, as an untagged AAudio stream.
    if cfg.game_audio {
        let ab = env.new_object(
            jni_str!("android/media/AudioAttributes$Builder"),
            jni_sig!("()V"),
            &[],
        )?;
        for (setter, value) in [
            (jni_str!("setUsage"), USAGE_GAME),
            (jni_str!("setContentType"), CONTENT_TYPE_MOVIE),
        ] {
            env.call_method(
                &ab,
                setter,
                jni_sig!("(I)Landroid/media/AudioAttributes$Builder;"),
                &[JValue::Int(value)],
            )?;
        }
        let attrs = env
            .call_method(
                &ab,
                jni_str!("build"),
                jni_sig!("()Landroid/media/AudioAttributes;"),
                &[],
            )?
            .l()?;
        env.call_method(
            &tb,
            jni_str!("setAudioAttributes"),
            jni_sig!("(Landroid/media/AudioAttributes;)Landroid/media/AudioTrack$Builder;"),
            &[JValue::Object(&attrs)],
        )?;
    }
    env.call_method(
        &tb,
        jni_str!("build"),
        jni_sig!("()Landroid/media/AudioTrack;"),
        &[],
    )?
    .l()
}

fn int(env: &mut Env, obj: &JObject, name: &JNIStr) -> JniResult<i32> {
    env.call_method(obj, name, jni_sig!("()I"), &[])?.i()
}

fn void(env: &mut Env, obj: &JObject, name: &JNIStr) -> JniResult<()> {
    env.call_method(obj, name, jni_sig!("()V"), &[]).map(drop)
}
