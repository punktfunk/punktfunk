//! PipeWire playback (decoded PCM → stream) and mic uplink (capture → Opus → 0xCB).
//!
//! Playback matches the host virtual-mic producer: the pump pushes one decoded
//! frame per arrival; PipeWire pulls quanta on the device clock. Prime ~3 quanta,
//! cap the ring, re-prime after a real drain.
//!
//! Opens at the session-negotiated [`PlaybackFormat`]: 48 kHz / 5 ms Opus (`0xC9`)
//! or 48/96 kHz PCM of 1–5 ms (`0xD3`, `design/hi-res-audio.md`). Graph format is
//! F32LE at every rate — core already decoded to f32 (process-callback `stride`).

use anyhow::{Context, Result};
use punktfunk_core::client::NativeClient;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;

/// Mic uplink only. libopus is 48 kHz; playback takes its rate from [`PlaybackFormat`].
const SAMPLE_RATE: u32 = 48_000;
/// Voice is mono at the source; the host Opus decoder upmixes. Half the samples, half the wire.
const MIC_CHANNELS: usize = 1;

struct Terminate;

/// PipeWire endpoint for the settings pickers.
#[derive(Clone, Debug)]
pub struct AudioDevice {
    /// `node.name` — streams target this via `target.object`.
    pub name: String,
    /// `node.description` — picker label.
    pub description: String,
}

/// Device output latency behind a playback stream, ns: the graph's delay to the device plus
/// what sits queued in the stream and buffered in its resampler. RT-safe; call from `process`.
fn output_latency_ns(stream: &pipewire::stream::Stream, stride: usize) -> u64 {
    use pipewire::sys::{pw_stream_get_time_n, pw_time};
    // SAFETY: `pw_time` is plain integers; all-zero is a valid value.
    let mut t: pw_time = unsafe { std::mem::zeroed() };
    // SAFETY: `stream` is live for this callback, and `t` is exactly the size passed.
    let r = unsafe {
        pw_stream_get_time_n(stream.as_raw_ptr(), &mut t, std::mem::size_of::<pw_time>())
    };
    if r < 0 || t.rate.denom == 0 {
        return 0;
    }
    let frames = t.delay.max(0) as u64 + t.queued / stride.max(1) as u64 + t.buffered;
    frames.saturating_mul(1_000_000_000 * u64::from(t.rate.num)) / u64::from(t.rate.denom)
}

/// `(sinks, sources)` from one bounded registry round. Caller treats `Err` as no pickers.
pub fn devices() -> Result<(Vec<AudioDevice>, Vec<AudioDevice>)> {
    use crate::pw_oneshot::{OneShot, TIMEOUT};
    use std::cell::RefCell;
    use std::rc::Rc;

    let session = OneShot::connect("audio-devices", TIMEOUT)?;
    let found: Rc<RefCell<(Vec<AudioDevice>, Vec<AudioDevice>)>> = Rc::default();
    let _reg_listener = session
        .registry
        .add_listener_local()
        .global({
            let found = found.clone();
            move |g| {
                let Some(props) = g.props else { return };
                let sink = match props.get("media.class") {
                    Some("Audio/Sink") => true,
                    Some("Audio/Source") => false,
                    _ => return,
                };
                let Some(name) = props.get("node.name") else {
                    return;
                };
                let description = props
                    .get("node.description")
                    .or_else(|| props.get("node.nick"))
                    .unwrap_or(name)
                    .to_string();
                let dev = AudioDevice {
                    name: name.to_string(),
                    description,
                };
                let mut f = found.borrow_mut();
                if sink { &mut f.0 } else { &mut f.1 }.push(dev);
            }
        })
        .register();

    session.round()?; // the globals replay
    let result = found.borrow().clone();
    Ok(result)
}

/// Format the session resolved from `Welcome` — never what the client asked for.
/// One value, not three `u32`s: transposing them would open a plausible wrong device.
///
/// Declared in both backends (`audio.rs` / `audio_wasapi.rs`) rather than shared:
/// the twins are picked by `lib.rs`'s `#[path]` and already spell out every other item.
#[derive(Clone, Copy, Debug)]
pub struct PlaybackFormat {
    /// Interleaved count (2/6/8). Wire order FL FR FC LFE RL RR SL SR.
    pub channels: u32,
    /// 48_000 on Opus; 48_000 or 96_000 on lossless (`design/hi-res-audio.md`).
    /// 44.1 kHz and its multiples are deferred: they truncate `JitterPolicy`'s integer samples-per-ms.
    pub rate_hz: u32,
    /// One protocol frame in µs: 5_000 on Opus; lossless from path MTU (4 ms at 48/24, 2 ms at 96/24).
    /// Sizes the graph quantum and the policy's shed/floor arithmetic.
    pub frame_us: u32,
}

impl PlaybackFormat {
    /// Frames per channel in one protocol frame — the graph quantum. In µs so 2_500 µs at 48 kHz is 120, not 96.
    fn quantum_frames(&self) -> u32 {
        ((self.rate_hz as u64 * self.frame_us as u64 / 1_000_000) as u32).max(1)
    }
}

pub struct AudioPlayer {
    pcm_tx: SyncSender<Vec<f32>>,
    /// Pool half of the pcm channel; see [`AudioPlayer::take_buffer`].
    recycle_rx: Receiver<Vec<f32>>,
    quit_tx: pipewire::channel::Sender<Terminate>,
    thread: Option<std::thread::JoinHandle<()>>,
    sync: Arc<punktfunk_core::audio::AudioSyncCell>,
    /// Callback publishes atomics only (realtime; it formats nothing). Decode thread logs them.
    vitals: Arc<crate::audio_vitals::PlaybackVitals>,
}

impl AudioPlayer {
    /// No PipeWire → `Err`; caller continues video-only.
    pub fn spawn(fmt: PlaybackFormat) -> Result<AudioPlayer> {
        // 64 chunks of slack (320 ms at Opus 5 ms, 128 ms at lossless 2 ms) — still above de-jitter.
        // Chunk COUNT, not scaled to frame duration, matching core `AUDIO_QUEUE`.
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        // Process callback returns drained Vecs so steady-state playback stops allocating.
        // Same capacity as the data channel; a full pool drops the Vec.
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        let sync: Arc<punktfunk_core::audio::AudioSyncCell> = Arc::default();
        let sync_cb = sync.clone();
        let vitals: Arc<crate::audio_vitals::PlaybackVitals> = Arc::default();
        let vitals_cb = vitals.clone();
        let thread = std::thread::Builder::new()
            .name("punktfunk-audio".into())
            .spawn(move || {
                if let Err(e) = pw_thread(pcm_rx, recycle_tx, quit_rx, fmt, sync_cb, vitals_cb) {
                    tracing::warn!(error = %e, "audio playback thread ended");
                }
            })
            .context("spawn audio thread")?;
        Ok(AudioPlayer {
            pcm_tx,
            recycle_rx,
            quit_tx,
            thread: Some(thread),
            sync,
            vitals,
        })
    }

    pub fn sync_cell(&self) -> Arc<punktfunk_core::audio::AudioSyncCell> {
        self.sync.clone()
    }

    pub fn vitals(&self) -> Arc<crate::audio_vitals::PlaybackVitals> {
        self.vitals.clone()
    }

    /// Empty, capacity intact. Allocates only when the pool is dry.
    pub fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    /// Drops the chunk if PipeWire is wedged; never blocks the session pump.
    pub fn push(&self, pcm: Vec<f32>) {
        if let Err(TrySendError::Disconnected(_)) = self.pcm_tx.try_send(pcm) {
            // Thread already dead — Drop reaps it.
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.quit_tx.send(Terminate);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// This backend's de-jitter numbers, named so the decode thread reads what the callback runs.
/// If they drift, one platform quietly carries a third of another's slack.
pub(crate) const TUNING: punktfunk_core::audio::JitterTuning =
    punktfunk_core::audio::JitterTuning::PIPEWIRE;

/// Gate on advertising `CLIENT_CAP_AUDIO_HIRES`: capable *and* the user turned it on
/// (`design/hi-res-audio.md`).
///
/// Always true here: a PipeWire playback stream declares its own format and the graph
/// inserts an adapter, so this client always opens at the resolved rate. The WASAPI
/// twin can fail — shared-mode autoconvert silently uses an engine format we do not control.
///
/// The sink may still resample (96 kHz into 48 kHz). Reading the sink rate needs a
/// registry lookup this crate does not do; `param_changed` logs what the graph granted.
pub fn can_render_at(_rate_hz: u32) -> bool {
    true
}

struct PlayerData {
    rx: Receiver<Vec<f32>>,
    recycle: SyncSender<Vec<f32>>,
    ring: VecDeque<f32>,
    policy: punktfunk_core::audio::JitterPolicy,
    channels: usize,
    /// Opened format; `param_changed` compares what the graph granted against it.
    fmt: PlaybackFormat,
    /// Last `param_changed` format, so a graph resume is not logged as a change.
    negotiated: Option<(u32, u32)>,
    sync: Arc<punktfunk_core::audio::AudioSyncCell>,
    vitals: Arc<crate::audio_vitals::PlaybackVitals>,
}

fn pw_thread(
    pcm_rx: Receiver<Vec<f32>>,
    recycle_tx: SyncSender<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    fmt: PlaybackFormat,
    sync: Arc<punktfunk_core::audio::AudioSyncCell>,
    vitals: Arc<crate::audio_vitals::PlaybackVitals>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    pw::init();

    let channels = fmt.channels as usize;

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context
        .connect_rc(None)
        .context("pw connect (is PipeWire running in this session?)")?;

    let _quit_guard = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    // One protocol frame (`<frames>/<rate>`). Both halves follow the session: a fixed
    // `"240/48000"` at 96 kHz would ask 2.5 ms at double the callback rate.
    // `properties!` takes literals only, so this is `insert`.
    let node_latency = format!("{}/{}", fmt.quantum_frames(), fmt.rate_hz);
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE       => "Audio",
        *pw::keys::MEDIA_CATEGORY   => "Playback",
        *pw::keys::MEDIA_ROLE       => "Game",
        *pw::keys::NODE_NAME        => "punktfunk-client",
        *pw::keys::NODE_DESCRIPTION => "Punktfunk Stream",
    };
    props.insert(*pw::keys::NODE_LATENCY, node_latency.as_str());
    // Settings speaker pick; unset/empty = PipeWire's default routing.
    if let Ok(target) = std::env::var("PUNKTFUNK_AUDIO_SINK") {
        if !target.is_empty() {
            // Raw key: `keys::TARGET_OBJECT` is feature-gated on a newer libpipewire than we require.
            props.insert("target.object", target);
        }
    }
    let stream =
        pw::stream::StreamBox::new(&core, "punktfunk-client", props).context("pw Stream")?;

    // Pre-reserved so `extend` never reallocates on the realtime loop. Cap plus the
    // 64-chunk channel of this plane's frame; sized from rate, frame, and channels.
    let ring_capacity = {
        let per_ms = fmt.rate_hz as usize * channels / 1000;
        let frame = punktfunk_core::audio::pcm::samples_per_frame(
            fmt.rate_hz,
            fmt.frame_us,
            fmt.channels as u8,
        );
        per_ms * TUNING.hard_cap_ms as usize + 64 * frame
    };
    let ud = PlayerData {
        rx: pcm_rx,
        recycle: recycle_tx,
        ring: VecDeque::with_capacity(ring_capacity),
        policy: {
            // Resolved format on both axes: `new_at_rate` for samples-per-ms, `set_frame_us`
            // for the floor and one-frame shed. Defaults at 96 kHz would shed 2.5 frames at a time.
            let mut p = punktfunk_core::audio::JitterPolicy::new_at_rate(
                TUNING,
                fmt.channels as u8,
                fmt.rate_hz,
            );
            p.set_frame_us(fmt.frame_us);
            p
        },
        channels,
        fmt,
        negotiated: None,
        sync,
        vitals,
    };

    let _listener = stream
        .add_local_listener_with_user_data(ud)
        .state_changed(|_s, _ud, old, new| {
            tracing::debug!(?old, ?new, "pipewire playback stream state");
        })
        // What the graph granted, not what we asked (`design/hi-res-audio.md`). This is OUR
        // port: a changed rate means the graph refused; an unchanged rate says nothing about
        // the sink. Sink rate is a registry lookup this stream does not do (`can_render_at`).
        .param_changed(|_s, ud, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = AudioInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            let now = (info.rate(), info.channels());
            // A resume re-announces the same format; that is the graph waking us, not a change.
            if ud.negotiated == Some(now) {
                return;
            }
            ud.negotiated = Some(now);
            if now.0 != 0 && now.0 != ud.fmt.rate_hz {
                tracing::warn!(
                    granted_hz = now.0,
                    resolved_hz = ud.fmt.rate_hz,
                    "PipeWire granted a different playback rate than the session negotiated — \
                     audio will be resampled and the A/V-sync depth arithmetic is denominated in \
                     the negotiated rate"
                );
            } else {
                tracing::info!(
                    format = ?info.format(),
                    rate = now.0,
                    channels = now.1,
                    "playback format negotiated"
                );
            }
        })
        // REALTIME (`RT_PROCESS` below): libpipewire data loop. No alloc, no locks a
        // lower-priority thread can hold, no logging (vitals as atomics). Without the
        // flag, `process` runs on our main loop and graph underruns never hit our counters.
        .process(|stream, ud| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                while let Ok(mut chunk) = ud.rx.try_recv() {
                    ud.ring.extend(chunk.iter().copied());
                    chunk.clear();
                    let _ = ud.recycle.try_send(chunk);
                }
                // `requested` is this cycle's quantum; the mapped buffer is sized for PipeWire's
                // `quantum-limit` (8192 ≈ 170 ms). Filling to capacity taught the policy a 170 ms
                // drain and lifted the underrun floor above any sync target. `requested == 0` → capacity.
                let requested = usize::try_from(buffer.requested()).unwrap_or(0);
                // F32LE at every rate and depth. Core already decoded 16/24-bit to f32; S24 would
                // rewrite this callback to carry bits that already fit in the f32 mantissa.
                let stride = 4 * ud.channels;
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let max_frames = data.data().map(|s| s.len() / stride).unwrap_or(0);
                let want_frames = if requested > 0 {
                    requested.min(max_frames)
                } else {
                    max_frames
                };
                let want = want_frames * ud.channels;
                // Once per stream: whether `requested` or the buffer ceiling sizes writes.
                if !ud.vitals.quantum_known() {
                    ud.vitals
                        .note_quantum(requested as u32, max_frames as u32, want_frames as u32);
                }

                // Continuity outranks sync: the policy clamps the request to its underrun floor (`set_sync_target`).
                ud.policy.set_sync_target(ud.sync.target());
                ud.sync.publish_depth(ud.ring.len());
                ud.sync
                    .publish_output_latency_ns(output_latency_ns(stream, stride));

                let step = ud.policy.step(ud.ring.len(), want);
                if step.drop_front > 0 {
                    punktfunk_core::audio::crossfade_drop(
                        &mut ud.ring,
                        step.drop_front,
                        step.crossfade,
                    );
                }
                // Sync asked for a deeper ring: one duplicated, crossfaded frame, not a de-prime.
                // Allocation-free: the ring is reserved for cap plus slack; policy inserts below target.
                if step.insert_front > 0 {
                    punktfunk_core::audio::crossfade_insert(
                        &mut ud.ring,
                        step.insert_front,
                        step.crossfade,
                    );
                }

                let mut ran_short = false;
                let n_frames = if let Some(slice) = data.data() {
                    for k in 0..want {
                        let s = if step.silence {
                            0.0
                        } else {
                            ud.ring.pop_front().unwrap_or_else(|| {
                                ran_short = true;
                                0.0
                            })
                        };
                        let off = k * 4;
                        slice[off..off + 4].copy_from_slice(&s.to_le_bytes());
                    }
                    want_frames
                } else {
                    0
                };
                // No-op while un-primed, so priming silence is never counted as an underrun.
                ud.policy.note_read(ran_short);
                ud.vitals.note_callback(
                    ran_short,
                    step.drop_front > 0,
                    step.insert_front > 0,
                    ud.policy.avg_depth_ms(),
                    ud.policy.target_ms(),
                );
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                *chunk.size_mut() = (stride * n_frames) as _;
            }));
            if outcome.is_err() {
                tracing::error!("panic in pipewire playback callback");
            }
        })
        .register()
        .context("register playback listener")?;

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(fmt.rate_hz);
    info.set_channels(channels as u32);
    info.set_position(punktfunk_core::audio::spa_positions(channels as u8));
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize format pod")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("pod from bytes")?];

    // `RT_PROCESS`: `process` on libpipewire's realtime data loop, not this main-loop thread.
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .context("pw stream connect")?;

    mainloop.run();
    tracing::debug!("pipewire playback loop exited");
    Ok(())
}

/// Capture → Opus 10 ms mono → 0xCB into the host virtual source.
pub struct MicStreamer {
    quit_tx: pipewire::channel::Sender<Terminate>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MicStreamer {
    /// `muted` is in-stream (B4): the callback keeps pulling and discards frames.
    /// Stopping the stream was rejected — unmute re-primes the device and re-runs
    /// source selection, so the first second back is glitchy.
    ///
    /// `echo_cancel` is the Settings toggle; `PUNKTFUNK_NO_AEC=1` overrides it off.
    pub fn spawn(
        connector: Arc<NativeClient>,
        muted: Arc<std::sync::atomic::AtomicBool>,
        echo_cancel: bool,
    ) -> Result<MicStreamer> {
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        let thread = std::thread::Builder::new()
            .name("punktfunk-mic".into())
            .spawn(move || {
                // Capture `process` runs here (no RT_PROCESS): encode and send are on this thread; a late tick is mic latency.
                crate::audio_rt::boost_and_log("punktfunk-mic");
                if let Err(e) = mic_thread(&connector, quit_rx, muted, echo_cancel) {
                    tracing::warn!(error = %e, "mic uplink thread ended");
                }
            })
            .context("spawn mic thread")?;
        Ok(MicStreamer {
            quit_tx,
            thread: Some(thread),
        })
    }
}

impl Drop for MicStreamer {
    fn drop(&mut self) {
        let _ = self.quit_tx.send(Terminate);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Encode-in-callback is fine: 10 ms Opus is well under 100 µs.
struct MicData {
    connector: Arc<NativeClient>,
    mic: punktfunk_core::audio::mic::MicEncoder,
    /// In-stream mute (B4); session chord flips it. Read per callback.
    muted: Arc<std::sync::atomic::AtomicBool>,
}

/// Settings toggle, with `PUNKTFUNK_NO_AEC=1` as a one-way override off.
/// The env var wins — escape hatch for a misbehaving canceller; nothing turns AEC back on.
fn aec_enabled(echo_cancel: bool) -> bool {
    echo_cancel && crate::env_on("PUNKTFUNK_NO_AEC") != Some(true)
}

/// Capture `target.object`: Settings pick (`PUNKTFUNK_AUDIO_SOURCE`) first, else
/// the first echo-cancelled source so a desktop already running `module-echo-cancel`
/// does not feed the downlink back into the host mic. `None` = PipeWire default.
///
/// Preference only: `pw_context_load_module` is not safely exposed in pipewire 0.9.
fn mic_capture_target(echo_cancel: bool) -> Option<String> {
    if let Ok(target) = std::env::var("PUNKTFUNK_AUDIO_SOURCE") {
        if !target.is_empty() {
            return Some(target);
        }
    }
    if !aec_enabled(echo_cancel) {
        return None;
    }
    let name = echo_cancel_source()?;
    tracing::info!(
        source = %name,
        "mic capture targets the echo-cancelled source (Echo cancellation off, or \
         PUNKTFUNK_NO_AEC=1, disables this)"
    );
    Some(name)
}

/// First `Audio/Source` whose name or description matches `module-echo-cancel`'s
/// convention (`echo-cancel-*`, "Echo-Cancel …"). Registry miss → none.
fn echo_cancel_source() -> Option<String> {
    let (_, sources) = devices().ok()?;
    sources.into_iter().find_map(|d| {
        let name = d.name.to_ascii_lowercase();
        let desc = d.description.to_ascii_lowercase();
        (name.contains("echo-cancel")
            || name.contains("echo_cancel")
            || desc.contains("echo-cancel")
            || desc.contains("echo cancel"))
        .then_some(d.name)
    })
}

fn mic_thread(
    connector: &Arc<NativeClient>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    muted: Arc<std::sync::atomic::AtomicBool>,
    echo_cancel: bool,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    pw::init();

    // The callback drains every frame it fills, so no backlog builds to self-heal.
    let mic = punktfunk_core::audio::mic::MicEncoder::new(false)
        .map_err(|e| anyhow::anyhow!("opus encoder: {e}"))?;

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw mic MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw mic Context")?;
    let core = context
        .connect_rc(None)
        .context("pw mic connect (is PipeWire running in this session?)")?;

    let _quit_guard = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE       => "Audio",
        *pw::keys::MEDIA_CATEGORY   => "Capture",
        *pw::keys::MEDIA_ROLE       => "Communication",
        *pw::keys::NODE_NAME        => "punktfunk-mic-capture",
        *pw::keys::NODE_DESCRIPTION => "Punktfunk Microphone",
        // One 10 ms mic frame. Without it the stream inherits the graph quantum (often
        // 1024–2048), so capture arrives in 21–43 ms bursts that sit ahead of the encoder.
        *pw::keys::NODE_LATENCY     => "480/48000",
    };
    if let Some(target) = mic_capture_target(echo_cancel) {
        // Raw key: `keys::TARGET_OBJECT` is feature-gated on a newer libpipewire than we require.
        props.insert("target.object", target);
    }
    let stream = pw::stream::StreamBox::new(&core, "punktfunk-mic-capture", props)
        .context("pw mic Stream")?;

    let ud = MicData {
        connector: connector.clone(),
        mic,
        muted,
    };

    let _listener = stream
        .add_local_listener_with_user_data(ud)
        .state_changed(|_s, _ud, old, new| {
            tracing::debug!(?old, ?new, "pipewire mic capture stream state");
        })
        .process(|stream, ud| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let n = data.chunk().size() as usize;
                if let Some(slice) = data.data() {
                    ud.mic.push(
                        slice[..n.min(slice.len())]
                            .chunks_exact(4)
                            .map(|s| f32::from_le_bytes([s[0], s[1], s[2], s[3]])),
                    );
                }
                // In-stream mute: the stream stays open so the device keeps primed buffers.
                let connector = &ud.connector;
                ud.mic.drain(
                    ud.muted.load(std::sync::atomic::Ordering::Relaxed),
                    |seq, pts, packet| {
                        let _ = connector.send_mic(seq, pts, packet.to_vec());
                    },
                );
            }));
            if outcome.is_err() {
                tracing::error!("panic in pipewire mic callback");
            }
        })
        .register()
        .context("register mic listener")?;

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    // Mono: the stream adapter downmixes whatever layout the source really has.
    info.set_channels(MIC_CHANNELS as u32);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize mic format pod")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("mic pod from bytes")?];

    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("pw mic stream connect")?;

    mainloop.run();
    tracing::debug!("pipewire mic capture loop exited");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(rate_hz: u32, frame_us: u32) -> PlaybackFormat {
        PlaybackFormat {
            channels: 2,
            rate_hz,
            frame_us,
        }
    }

    /// `NODE_LATENCY` is `<frames>/<rate>`; both halves follow the session.
    /// 48 kHz / 5 ms must stay `240` — every Opus session depends on that quantum.
    #[test]
    fn the_graph_quantum_is_one_protocol_frame_at_every_rung() {
        for (rate, us, want) in [
            (48_000, 5_000, 240),
            (48_000, 4_000, 192),
            // 2.5 ms at 48 kHz is 120 frames; whole-ms math would truncate to 96.
            (48_000, 2_500, 120),
            (96_000, 5_000, 480),
            (96_000, 3_000, 288),
            (96_000, 2_000, 192),
        ] {
            assert_eq!(fmt(rate, us).quantum_frames(), want, "{rate} Hz / {us} µs");
        }
    }

    /// A zero quantum would be a graph ask of nothing; clamp to 1.
    #[test]
    fn a_nonsense_format_still_asks_for_a_frame() {
        assert_eq!(fmt(0, 0).quantum_frames(), 1);
        assert_eq!(fmt(0, 5_000).quantum_frames(), 1);
    }
}
