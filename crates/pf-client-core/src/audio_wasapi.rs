//! Audio: playback (decoded PCM → a WASAPI shared-mode render stream) and the microphone
//! uplink (WASAPI capture → Opus → 0xCB datagrams, the inverse of the host's virtual mic).
//!
//! WASAPI twin of `audio.rs` (PipeWire). Same public surface (`AudioPlayer::spawn` /
//! `take_buffer` / `push`, `MicStreamer::spawn`); `lib.rs` `#[path]`s one in as `crate::audio`.
//! COM objects are apartment-bound and not `Send`, so they live on a dedicated thread;
//! only the channels, stop flag, and join handle cross.
//!
//! Playback opens at the session-negotiated [`PlaybackFormat`], not a constant: 48 kHz Opus
//! on `0xC9`, or 48/96 kHz lossless PCM on `0xD3` (`design/hi-res-audio.md`). Shared-mode
//! `autoconvert` silently downsamples an over-rate stream — [`can_render_at`] is the gate.
//! Depth policy is shared `JitterPolicy` (`JitterTuning::WASAPI`).

use anyhow::{anyhow, Context, Result};
use punktfunk_core::client::NativeClient;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;
use wasapi::{
    AudioClientProperties, DeviceEnumerator, Direction, SampleType, StreamCategory, StreamMode,
    WaveFormat,
};

/// Mic uplink rate. libopus is 48 kHz by construction; playback takes its rate from Welcome.
const SAMPLE_RATE: usize = 48_000;
/// Capture stereo: WASAPI autoconvert matrixes any endpoint layout into the requested format.
/// Downmix to mono in code before encode — voice is mono, half the samples, half the wire.
const CAPT_CHANNELS: usize = 2;

/// Named once so the decode thread and the render loop share the same de-prime fuse.
pub(crate) const TUNING: punktfunk_core::audio::JitterTuning =
    punktfunk_core::audio::JitterTuning::WASAPI;

#[derive(Clone, Debug)]
pub struct AudioDevice {
    /// `IMMDevice` endpoint id — the stable key. PipeWire stores `node.name` in the same field.
    pub name: String,
    /// Friendly name shown in the picker ("Speakers (Realtek ...)").
    pub description: String,
}

/// Active endpoints `(sinks, sources)`. Runs on a short-lived MTA thread: the caller is
/// typically STA, and `CoInitializeEx(MTA)` there fails with `RPC_E_CHANGED_MODE`.
pub fn devices() -> Result<(Vec<AudioDevice>, Vec<AudioDevice>)> {
    std::thread::Builder::new()
        .name("pf-audio-enum".into())
        .spawn(|| -> Result<(Vec<AudioDevice>, Vec<AudioDevice>)> {
            wasapi::initialize_mta()
                .ok()
                .context("CoInitializeEx (MTA)")?;
            let enumerator = DeviceEnumerator::new().context("DeviceEnumerator")?;
            let mut out = (Vec::new(), Vec::new());
            for (direction, list) in [
                (Direction::Render, &mut out.0),
                (Direction::Capture, &mut out.1),
            ] {
                let coll = enumerator
                    .get_device_collection(&direction)
                    .context("device collection")?;
                for i in 0..coll.get_nbr_devices().context("device count")? {
                    // One broken endpoint (driver limbo) must not hide the rest.
                    let Ok(dev) = coll.get_device_at_index(i) else {
                        continue;
                    };
                    let (Ok(id), Ok(name)) = (dev.get_id(), dev.get_friendlyname()) else {
                        continue;
                    };
                    list.push(AudioDevice {
                        name: id,
                        description: name,
                    });
                }
            }
            Ok(out)
        })
        .context("spawn audio enumeration thread")?
        .join()
        .map_err(|_| anyhow!("audio enumeration thread panicked"))?
}

/// Resolve an active endpoint by id. Do not call `DeviceEnumerator::get_device` — wasapi 0.23
/// UAF'd the temporary `HSTRING`, and a collection scan also drops inactive endpoints.
/// Host uses raw COM for the same bug; this crate's `windows` rev is incompatible with wasapi's.
pub(crate) fn device_by_id(
    enumerator: &DeviceEnumerator,
    direction: &Direction,
    id: &str,
) -> Result<wasapi::Device> {
    let devices = enumerator
        .get_device_collection(direction)
        .map_err(|e| anyhow!("enumerate {direction:?} endpoints: {e}"))?;
    let count = devices
        .get_nbr_devices()
        .map_err(|e| anyhow!("endpoint count: {e}"))?;
    for i in 0..count {
        let dev = devices
            .get_device_at_index(i)
            .map_err(|e| anyhow!("endpoint {i}: {e}"))?;
        if dev.get_id().is_ok_and(|got| got == id) {
            return Ok(dev);
        }
    }
    anyhow::bail!("no active {direction:?} endpoint with id {id}")
}

fn pick_device(
    enumerator: &DeviceEnumerator,
    direction: &Direction,
    var: &str,
) -> Result<wasapi::Device> {
    if let Some(id) = std::env::var(var).ok().filter(|v| !v.is_empty()) {
        match device_by_id(enumerator, direction, &id) {
            Ok(d) => {
                tracing::info!(
                    var,
                    endpoint = %d.get_friendlyname().unwrap_or_else(|_| id.clone()),
                    "using the picked audio endpoint"
                );
                return Ok(d);
            }
            Err(e) => tracing::warn!(
                var,
                endpoint_id = %id,
                error = %e,
                "picked audio endpoint not found — using the default"
            ),
        }
    }
    enumerator
        .get_default_device(direction)
        .context("default endpoint")
}

/// Format the session resolved on `Welcome` — never what the client asked. One value, not
/// three positional `u32`s: transposing them opens the endpoint at a plausible wrong format.
#[derive(Clone, Copy, Debug)]
pub struct PlaybackFormat {
    /// Interleaved channel count (2/6/8), wire order FL FR FC LFE RL RR SL SR.
    pub channels: u32,
    /// Negotiated rate: 48 000 on Opus, 48 000 or 96 000 on lossless (`design/hi-res-audio.md`).
    /// 44.1 kHz is deferred — `JitterPolicy` uses integer samples-per-millisecond.
    pub rate_hz: u32,
    /// One protocol frame in microseconds. Feeds the policy's shed/floor, which is in frames.
    pub frame_us: u32,
}

/// The render endpoint's engine mix rate, or `None` if nothing readable answered.
///
/// Shared-mode `autoconvert: true` makes the engine rate authoritative: a 96 kHz stream into
/// a 48 kHz engine succeeds, returns no error, and plays downsampled samples. Twin of the
/// capture trap in `design/hi-res-audio.md`.
///
/// Short-lived MTA thread: the session pump's COM apartment is not ours to claim.
fn render_engine_rate_hz() -> Option<u32> {
    std::thread::Builder::new()
        .name("pf-audio-engine".into())
        .spawn(|| -> Option<u32> {
            if wasapi::initialize_mta().ok().is_err() {
                return None;
            }
            let enumerator = DeviceEnumerator::new().ok()?;
            // The endpoint the render thread will pick, not the default — a USB DAC and the
            // system default routinely run at different rates.
            let device =
                pick_device(&enumerator, &Direction::Render, "PUNKTFUNK_AUDIO_SINK").ok()?;
            let client = device.get_iaudioclient().ok()?;
            client.get_mixformat().ok().map(|f| f.get_samplespersec())
        })
        .ok()?
        .join()
        .ok()?
}

/// Gate on advertising `CLIENT_CAP_AUDIO_HIRES`. Decline if the engine mix rate is below
/// `rate_hz` or unknown — autoconvert would downsample with no error. Do not drive the
/// engine format from the client; raise it in Windows' device properties.
///
/// Blocks on COM. The early return keeps ordinary 48 kHz sessions off the endpoint.
pub fn can_render_at(rate_hz: u32) -> bool {
    if rate_hz <= SAMPLE_RATE as u32 {
        return true;
    }
    match render_engine_rate_hz() {
        Some(hz) if hz >= rate_hz => true,
        Some(hz) => {
            tracing::warn!(
                engine_hz = hz,
                requested = rate_hz,
                "the render endpoint's audio engine runs below the requested rate — not asking \
                 for lossless audio, because WASAPI's shared-mode autoconvert would downsample it \
                 on arrival and the bandwidth would buy nothing (raise the rate in Windows' Sound \
                 → Device properties → Advanced to change this)"
            );
            false
        }
        None => {
            tracing::warn!(
                requested = rate_hz,
                "the render endpoint would not report its engine mix format — not asking for \
                 lossless audio, because there is no way to tell whether it would be downsampled \
                 on arrival"
            );
            false
        }
    }
}

pub struct AudioPlayer {
    pcm_tx: SyncSender<Vec<f32>>,
    recycle_rx: Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    sync: Arc<punktfunk_core::audio::AudioSyncCell>,
    vitals: Arc<crate::audio_vitals::PlaybackVitals>,
}

impl AudioPlayer {
    /// Failure (no render endpoint) is survivable — the caller streams video-only.
    pub fn spawn(fmt: PlaybackFormat) -> Result<AudioPlayer> {
        // 64 chunks of slack — 320 ms at Opus 5 ms, 128 ms at lossless 2 ms. A chunk COUNT,
        // not scaled to the negotiated frame, matching core's `AUDIO_QUEUE`.
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        // Render thread returns drained Vecs; a full pool drops them. Avoids ~200 allocs/s.
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<Option<u32>>>(1);
        let stop_t = stop.clone();
        let sync: Arc<punktfunk_core::audio::AudioSyncCell> = Arc::default();
        let sync_t = sync.clone();
        let vitals: Arc<crate::audio_vitals::PlaybackVitals> = Arc::default();
        let vitals_t = vitals.clone();
        let thread = std::thread::Builder::new()
            .name("punktfunk-audio".into())
            .spawn(move || {
                if let Err(e) =
                    render_thread(pcm_rx, recycle_tx, stop_t, ready_tx, fmt, sync_t, vitals_t)
                {
                    tracing::warn!(error = %format!("{e:#}"), "audio playback thread ended");
                }
            })
            .context("spawn audio thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(engine_hz)) => {
                tracing::info!(
                    channels = fmt.channels,
                    rate_hz = fmt.rate_hz,
                    frame_us = fmt.frame_us,
                    engine_hz,
                    "WASAPI render: 32-bit float"
                );
                Ok(AudioPlayer {
                    pcm_tx,
                    recycle_rx,
                    stop,
                    thread: Some(thread),
                    sync,
                    vitals,
                })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // A late open must not leave a thread playing into nothing for the process.
                stop.store(true, Ordering::SeqCst);
                Err(anyhow!(
                    "wasapi render init timed out (no render endpoint?)"
                ))
            }
        }
    }

    /// Recycled chunk Vec with capacity intact. Allocates only when the pool is dry.
    pub fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    pub fn sync_cell(&self) -> Arc<punktfunk_core::audio::AudioSyncCell> {
        self.sync.clone()
    }

    pub fn vitals(&self) -> Arc<crate::audio_vitals::PlaybackVitals> {
        self.vitals.clone()
    }

    /// Drop if the WASAPI side is wedged; never block the pump.
    pub fn push(&self, pcm: Vec<f32>) {
        if let Err(TrySendError::Disconnected(_)) = self.pcm_tx.try_send(pcm) {
            // Thread already dead — Drop will reap it.
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A running shared-mode stream signals every engine period (~10 ms); this many silent 100 ms
/// waits in a row mean the endpoint is gone without an error (unplug, exclusive grab).
const EVENT_SILENT_WAITS: u32 = 10;
/// How often a stream on the default endpoint checks that the default is still it.
const DEFAULT_CHECK_EVERY: Duration = Duration::from_secs(1);
/// Settle between losing an endpoint and reopening: a route change is not instantaneous.
const REOPEN_SETTLE: Duration = Duration::from_millis(250);
/// Reopens that may fail back to back before playback gives up (~2 s).
const REOPEN_ATTEMPTS: u32 = 8;

/// One opened render endpoint. COM objects: they stay on the render thread.
struct Endpoint {
    client: wasapi::AudioClient,
    render: wasapi::AudioRenderClient,
    event: wasapi::Handle,
    engine_hz: Option<u32>,
    name: String,
    /// Endpoint id, for noticing that the default moved to another device.
    id: Option<String>,
    /// No picked endpoint: this stream follows the default when it changes.
    follows_default: bool,
}

/// Why [`play`] returned.
enum Played {
    Stopped,
    Reopen(&'static str),
}

/// Own the render endpoint for the session: open it, play into it, and open it again when it
/// goes away (a Bluetooth disconnect, a USB DAC pulled, an exclusive-mode grab) or when the
/// default output moves. The ring and its policy carry over. Only the first open answers
/// [`AudioPlayer::spawn`]; a failure there means the session streams video-only.
fn render_thread(
    pcm_rx: Receiver<Vec<f32>>,
    recycle_tx: SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    ready: SyncSender<Result<Option<u32>>>,
    fmt: PlaybackFormat,
    sync: Arc<punktfunk_core::audio::AudioSyncCell>,
    vitals: Arc<crate::audio_vitals::PlaybackVitals>,
) -> Result<()> {
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    // A missed period is a click the ring cannot conceal. Best-effort MMCSS (`audio_rt`).
    crate::audio_rt::boost_and_log("wasapi-render");
    let channels = fmt.channels.clamp(1, 8) as u8;
    let mut ring: VecDeque<f32> = VecDeque::new();
    // Resolved rate + frame_us: defaults would shed 2.5 frames at a time at 96 kHz.
    let mut policy =
        punktfunk_core::audio::JitterPolicy::new_at_rate(TUNING, channels, fmt.rate_hz);
    policy.set_frame_us(fmt.frame_us);
    let mut ready = Some(ready);
    let mut failures: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        let ep = match open_render(fmt, channels) {
            Ok(ep) => ep,
            Err(e) => {
                if let Some(r) = ready.take() {
                    let _ = r.send(Err(anyhow!("{e:#}")));
                    return Ok(());
                }
                failures += 1;
                if failures > REOPEN_ATTEMPTS {
                    return Err(e.context("reopen the render endpoint"));
                }
                sleep_unless_stopped(&stop, REOPEN_SETTLE);
                continue;
            }
        };
        failures = 0;
        match ready.take() {
            Some(r) => {
                let _ = r.send(Ok(ep.engine_hz));
            }
            None => tracing::info!(endpoint = %ep.name, "WASAPI render reopened"),
        }
        let played = play(
            &ep,
            &pcm_rx,
            &recycle_tx,
            &stop,
            &sync,
            &vitals,
            &mut ring,
            &mut policy,
            fmt,
        );
        ep.client.stop_stream().ok();
        match played {
            Ok(Played::Stopped) => break,
            Ok(Played::Reopen(why)) => {
                tracing::info!(endpoint = %ep.name, why, "WASAPI render reopening")
            }
            Err(e) => tracing::warn!(endpoint = %ep.name, error = %format!("{e:#}"),
                "WASAPI render failed — reopening"),
        }
        sleep_unless_stopped(&stop, REOPEN_SETTLE);
    }
    Ok(())
}

fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) {
    let until = std::time::Instant::now() + total;
    while std::time::Instant::now() < until && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Open and start a shared-mode render stream at the session's format.
fn open_render(fmt: PlaybackFormat, channels: u8) -> Result<Endpoint> {
    let enumerator = DeviceEnumerator::new().context("DeviceEnumerator")?;
    let follows_default = std::env::var("PUNKTFUNK_AUDIO_SINK").map_or(true, |v| v.is_empty());
    let device = pick_device(&enumerator, &Direction::Render, "PUNKTFUNK_AUDIO_SINK")
        .context("render endpoint")?;
    let name = device.get_friendlyname().unwrap_or_default();
    let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
    // Engine mix format before init. Report, not a gate — the wire format is already
    // negotiated; declining here is silence. The gate is `can_render_at`. A mismatch
    // means the endpoint changed under us (unplug, shared-mode rate change).
    let engine_hz = audio_client
        .get_mixformat()
        .ok()
        .map(|f| f.get_samplespersec())
        .filter(|&hz| hz > 0);
    if let Some(hz) = engine_hz {
        if hz < fmt.rate_hz {
            tracing::warn!(
                engine_hz = hz,
                stream_hz = fmt.rate_hz,
                endpoint = %name,
                "the render endpoint's audio engine runs BELOW this session's negotiated \
                 rate — WASAPI's shared-mode autoconvert is downsampling every frame on \
                 arrival, so the extra bandwidth is being spent for nothing (raise the rate \
                 in Windows' Sound → Device properties → Advanced, then reconnect)"
            );
        }
    } else if fmt.rate_hz != SAMPLE_RATE as u32 {
        tracing::warn!(
            stream_hz = fmt.rate_hz,
            "the render endpoint would not report its engine mix format — there is no way to \
             tell whether this session's audio is being downsampled on arrival"
        );
    }
    // dwChannelMask is the wire order (5.1 = 0x3F, 7.1 = 0x63F). WASAPI delivers in
    // ascending mask-bit order, so the mapping is identity. Autoconvert downmixes.
    let desired = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        fmt.rate_hz as usize,
        channels as usize,
        Some(punktfunk_core::audio::wasapi_channel_mask(channels)),
    );
    let (default_period, _min_period) =
        audio_client.get_device_period().context("device period")?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: default_period,
    };
    audio_client
        .initialize_client(&desired, &Direction::Render, &mode)
        .context("initialize render client")?;
    let event = audio_client.set_get_eventhandle().context("event handle")?;
    let render = audio_client
        .get_audiorenderclient()
        .context("IAudioRenderClient")?;
    audio_client.start_stream().context("start render stream")?;
    Ok(Endpoint {
        client: audio_client,
        render,
        event,
        engine_hz,
        name,
        id: device.get_id().ok(),
        follows_default,
    })
}

/// Play the ring into `ep` until stop, a render error, or the endpoint going away.
#[allow(clippy::too_many_arguments)] // one call site, `render_thread`
fn play(
    ep: &Endpoint,
    pcm_rx: &Receiver<Vec<f32>>,
    recycle_tx: &SyncSender<Vec<f32>>,
    stop: &AtomicBool,
    sync: &punktfunk_core::audio::AudioSyncCell,
    vitals: &crate::audio_vitals::PlaybackVitals,
    ring: &mut VecDeque<f32>,
    policy: &mut punktfunk_core::audio::JitterPolicy,
    fmt: PlaybackFormat,
) -> Result<Played> {
    let channels = fmt.channels.clamp(1, 8) as usize;
    // f32 interleaved at every rate. Core already decoded 16/24-bit to f32; a 24-bit
    // WASAPI integer format would rewrite the ring, crossfade, and policy arithmetic.
    let block_align = channels * 4;
    let enumerator = DeviceEnumerator::new().context("DeviceEnumerator")?;
    let mut out = Vec::new();
    let mut silent_waits: u32 = 0;
    let mut default_checked = std::time::Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if ep.event.wait_for_event(100).is_err() {
            silent_waits += 1;
            if silent_waits >= EVENT_SILENT_WAITS {
                return Ok(Played::Reopen("the render event stopped"));
            }
            continue;
        }
        silent_waits = 0;
        if ep.follows_default && default_checked.elapsed() >= DEFAULT_CHECK_EVERY {
            default_checked = std::time::Instant::now();
            let now = enumerator
                .get_default_device(&Direction::Render)
                .ok()
                .and_then(|d| d.get_id().ok());
            if now.is_some() && now != ep.id {
                return Ok(Played::Reopen("the default output changed"));
            }
        }
        while let Ok(mut chunk) = pcm_rx.try_recv() {
            ring.extend(chunk.iter().copied());
            chunk.clear();
            let _ = recycle_tx.try_send(chunk);
        }
        let avail_frames = ep
            .client
            .get_available_space_in_frames()
            .context("available space")? as usize;
        if avail_frames == 0 {
            continue;
        }
        let want = avail_frames * channels;
        // First quantum is the engine period; decode thread prints it.
        if !vitals.quantum_known() {
            vitals.note_quantum(
                avail_frames as u32,
                avail_frames as u32,
                avail_frames as u32,
            );
        }

        // Policy clamps the sync request against its underrun floor: continuity outranks
        // alignment.
        policy.set_sync_target(sync.target());
        sync.publish_depth(ring.len());
        // What this write lands behind: the frames the endpoint already holds.
        let padding = u64::from(ep.client.get_current_padding().unwrap_or(0));
        sync.publish_output_latency_ns(padding * 1_000_000_000 / u64::from(fmt.rate_hz.max(1)));

        let step = policy.step(ring.len(), want);
        if step.drop_front > 0 {
            punktfunk_core::audio::crossfade_drop(ring, step.drop_front, step.crossfade);
        }
        // Deeper-ring request: duplicate one crossfaded frame, do not de-prime.
        if step.insert_front > 0 {
            punktfunk_core::audio::crossfade_insert(ring, step.insert_front, step.crossfade);
        }

        out.clear();
        out.resize(avail_frames * block_align, 0);
        let mut ran_short = false;
        if !step.silence {
            for dst in out.chunks_exact_mut(4) {
                let s = ring.pop_front().unwrap_or_else(|| {
                    ran_short = true;
                    0.0
                });
                dst.copy_from_slice(&s.to_le_bytes());
            }
        }
        // Unprimed silence is ignored, so priming is not counted as an underrun.
        policy.note_read(ran_short);
        vitals.note_callback(
            ran_short,
            step.drop_front > 0,
            step.insert_front > 0,
            policy.avg_depth_ms(),
            policy.target_ms(),
        );
        ep.render
            .write_to_device(avail_frames, &out, None)
            .context("write_to_device")?;
    }
    Ok(Played::Stopped)
}

/// Capture → Opus 10 ms mono → 0xCB into the host's virtual mic.
pub struct MicStreamer {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MicStreamer {
    /// `muted` is live with the capture loop (B4): keep reading, send nothing. Do not stop
    /// the `IAudioClient` — stop/start re-primes buffers and re-runs category negotiation.
    /// `echo_cancel` is the Settings toggle; `PUNKTFUNK_NO_AEC=1` overrides it off.
    pub fn spawn(
        connector: Arc<NativeClient>,
        muted: Arc<AtomicBool>,
        echo_cancel: bool,
    ) -> Result<MicStreamer> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let thread = std::thread::Builder::new()
            .name("punktfunk-mic".into())
            .spawn(move || {
                if let Err(e) = mic_thread(&connector, stop_t, muted, echo_cancel) {
                    tracing::warn!(error = %format!("{e:#}"), "mic uplink thread ended");
                }
            })
            .context("spawn mic thread")?;
        Ok(MicStreamer {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for MicStreamer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Settings toggle with `PUNKTFUNK_NO_AEC=1` as a one-way override off. The hook is the
/// Communications stream category; the PipeWire twin gates its echo-cancelled source the
/// same way.
fn aec_enabled(echo_cancel: bool) -> bool {
    echo_cancel && !std::env::var("PUNKTFUNK_NO_AEC").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn mic_thread(
    connector: &Arc<NativeClient>,
    stop: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    echo_cancel: bool,
) -> Result<()> {
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")?;
    crate::audio_rt::boost_and_log("wasapi-mic");

    // Each event drains the bounded endpoint buffer, so no backlog builds to self-heal.
    let mut mic = punktfunk_core::audio::mic::MicEncoder::new(false)
        .map_err(|e| anyhow!("opus encoder: {e}"))?;

    let enumerator = DeviceEnumerator::new().context("DeviceEnumerator")?;
    let device = pick_device(&enumerator, &Direction::Capture, "PUNKTFUNK_AUDIO_SOURCE")
        .context("capture endpoint (no microphone?)")?;
    let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
    // Communications category is the only way a driver/APO echo canceller engages.
    // Must precede Initialize (`SetClientProperties` is pre-init). Best-effort without
    // IAudioClient2. Opt-out is `aec_enabled`.
    if aec_enabled(echo_cancel) {
        if let Err(e) = audio_client.set_properties(
            AudioClientProperties::new().set_category(StreamCategory::Communications),
        ) {
            tracing::debug!(error = %e, "mic capture: Communications category not set");
        }
    }
    let desired = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE, CAPT_CHANNELS, None);
    let (default_period, _min_period) =
        audio_client.get_device_period().context("device period")?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: default_period,
    };
    audio_client
        .initialize_client(&desired, &Direction::Capture, &mode)
        .context("initialize capture client")?;
    let h_event = audio_client.set_get_eventhandle().context("event handle")?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .context("IAudioCaptureClient")?;
    audio_client
        .start_stream()
        .context("start capture stream")?;

    let mut bytes: VecDeque<u8> = VecDeque::new();

    while !stop.load(Ordering::Relaxed) {
        if h_event.wait_for_event(100).is_err() {
            continue;
        }
        loop {
            match capture_client.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(_n)) => {
                    capture_client
                        .read_from_device_to_deque(&mut bytes)
                        .context("read capture")?;
                }
                Err(e) => return Err(anyhow!("get_next_packet_size: {e}")),
            }
        }
        // Autoconvert already matrixed the endpoint layout to stereo; average L/R to mono.
        let stereo_frame = 4 * CAPT_CHANNELS;
        let whole = (bytes.len() / stereo_frame) * stereo_frame;
        let stereo: Vec<u8> = bytes.drain(..whole).collect();
        mic.push(stereo.chunks_exact(stereo_frame).map(|c| {
            let l = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            let r = f32::from_le_bytes([c[4], c[5], c[6], c[7]]);
            (l + r) * 0.5
        }));
        // Muted, the client stays started and frames drop unencoded.
        mic.drain(muted.load(Ordering::Relaxed), |seq, pts, packet| {
            let _ = connector.send_mic(seq, pts, packet.to_vec());
        });
    }
    audio_client.stop_stream().ok();
    Ok(())
}
