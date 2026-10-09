//! PipeWire desktop-audio capture through a host-owned virtual sink.
//!
//! Default (`PUNKTFUNK_STREAM_SINK` unset): create a `support.null-audio-sink`
//! adapter and capture its monitor. That node is a **driver** (`timerfd` in the
//! daemon data loop), so the capture group owns its clock. A stream node is a
//! follower; PipeWire would schedule the group on any running driver on the box.
//!
//! `=stream`: the capture stream itself is the `Audio/Sink`. Same routing, but
//! the group borrows a driver. One-release escape hatch.
//!
//! `=0`: follow the default sink's monitor. Coupled to hardware-default churn.
//! Both sink modes advertise the session channel count, so a game can produce
//! 5.1/7.1 when local hardware is stereo.
//!
//! `!Send` MainLoop/Stream live on a dedicated thread; interleaved `f32` leaves
//! over a bounded channel (drop, never block the loop). Drop quits the loop
//! and tears the sink so a surround session can replace a stereo capturer
//! without leaking a consumer (a wedged link head-blocks the daemon).

mod host_bridge;
mod mic;
mod monitor_rate;
mod pad_card_volume;
pub mod pad_sink;
mod playing_apps;
pub use playing_apps::playing_apps;
pub mod pad_usb;
mod pw_oneshot;
mod pw_setup;
mod stream_sink;

pub(super) fn claim_default_mic() {
    stream_sink::SOURCE.claim(stream_sink::MIC_NAME);
}

pub(super) fn release_default_mic() {
    stream_sink::SOURCE.release(stream_sink::MIC_NAME);
}

pub(super) fn restore_defaults() {
    stream_sink::SINK.release_all();
    stream_sink::SOURCE.release_all();
    host_bridge::release_all_pins();
}

pub(super) fn heal_defaults() {
    stream_sink::SINK.heal();
    stream_sink::SOURCE.heal();
}

use super::{AudioCapturer, VirtualMic, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use punktfunk_core::audio::{spa_channel_order, spa_positions};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

struct Terminate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    /// Host-created `support.null-audio-sink` — a driver of its own group — plus its monitor. Default.
    NullSink,
    /// Capture stream is the `Audio/Sink`; the group borrows a driver. Escape hatch.
    StreamSink,
    /// Tap the default sink's monitor; no host-owned sink.
    Monitor,
}

impl CaptureMode {
    fn owns_sink(self) -> bool {
        !matches!(self, CaptureMode::Monitor)
    }

    fn as_str(self) -> &'static str {
        match self {
            CaptureMode::NullSink => "null-sink",
            CaptureMode::StreamSink => "stream-sink",
            CaptureMode::Monitor => "monitor",
        }
    }
}

/// Whether capture owns a per-capturer sink that isolation can env-route into.
/// Monitor mode (`PUNKTFUNK_STREAM_SINK=0`) has none, so isolation's audio
/// half degrades to shared.
pub fn sink_capture_active() -> bool {
    capture_mode().owns_sink()
}

fn capture_mode() -> CaptureMode {
    if crate::capture_policy::session_keeps_default()
        || pf_host_config::config().audio_output_mode.keeps_default()
    {
        // `CLIENT_CAP_KEEP_HOST_AUDIO` or `audio.output_mode = follow_default`:
        // follow the operator's default sink, no default-sink claim. Wins over
        // `PUNKTFUNK_STREAM_SINK` — "don't touch my devices" is the more
        // restrictive promise.
        return CaptureMode::Monitor;
    }
    capture_mode_from(std::env::var("PUNKTFUNK_STREAM_SINK").ok().as_deref())
}

/// What an open read from the audio settings. A parked capturer serves the next session only
/// while this still holds: a keep-host session must not inherit a sink claim.
#[derive(Debug, PartialEq)]
struct OpenPolicy {
    mode: CaptureMode,
    output: pf_host_config::AudioOutputMode,
    voice: pf_host_config::VoiceChatRoute,
    voice_apps: Vec<String>,
}

impl OpenPolicy {
    fn now() -> OpenPolicy {
        let cfg = pf_host_config::config();
        OpenPolicy {
            mode: capture_mode(),
            output: cfg.audio_output_mode,
            voice: cfg.audio_voice_chat,
            voice_apps: cfg.audio_voice_apps.clone(),
        }
    }
}

/// Env grammar without process-global mutation, so the three modes are testable.
/// Unrecognised values (and unset) are NullSink: a typo must not kill audio.
fn capture_mode_from(value: Option<&str>) -> CaptureMode {
    match value.map(str::trim) {
        Some("0" | "false" | "no" | "off") => CaptureMode::Monitor,
        Some("stream") => CaptureMode::StreamSink,
        _ => CaptureMode::NullSink,
    }
}

#[derive(Debug, Clone)]
struct CaptureNodes {
    mode: CaptureMode,
    /// `Audio/Sink` `node.name` in both sink modes; the [`stream_sink`] claim
    /// target. StreamSink: this IS the capture stream. NullSink: the adapter
    /// whose monitor [`capture`](Self::capture) taps.
    sink: Option<String>,
    /// Capture stream `node.name`. Same as [`sink`](Self::sink) only in StreamSink.
    capture: String,
    /// Monitor mode: the sink whose monitor to tap. `None` taps the default sink.
    target: Option<String>,
}

/// Linux capture-rate answer for the hi-res gate (`design/hi-res-audio.md`).
///
/// Both sink modes declare the `Audio/Sink` format (`audio.rate` on the
/// adapter, or the stream's negotiated format), so the rate we claim is the
/// rate we get — [`Declared`](super::CaptureRate::Declared), no probe.
///
/// Monitor mode (`PUNKTFUNK_STREAM_SINK=0`) captures through PipeWire's
/// resampler, which reports a clean rate whatever the node upstream really
/// runs at. The answer comes from the monitored node via
/// [`monitor_rate::monitored_sink_rate`], as an
/// [`Engine`](super::CaptureRate::Engine) rate.
///
/// Unreadable (suspended, missing key, timeout) is
/// [`Unknown`](super::CaptureRate::Unknown) and declines. A wrong guess would
/// advertise 96 kHz while carrying interpolated 48 kHz with both ends
/// auditing clean — so this never guesses: no graph-default, no `EnumFormat`
/// (capability, not fact), no fallback to the rate we asked for.
pub(super) fn probe_capture_rate() -> super::CaptureRate {
    if capture_mode().owns_sink() {
        return super::CaptureRate::Declared;
    }
    match monitor_rate::monitored_sink_rate() {
        Ok(rate_hz) => {
            tracing::debug!(
                rate_hz,
                "hi-res capture-rate probe: the sink this host would monitor runs at this rate"
            );
            super::CaptureRate::Engine(rate_hz)
        }
        Err(e) => {
            tracing::debug!(
                reason = %format!("{e:#}"),
                "hi-res capture-rate probe: the monitored sink's own rate is not readable — \
                 declining hi-res (PUNKTFUNK_STREAM_SINK=0 captures through PipeWire's resampler, \
                 so the rate our own stream reports proves nothing)"
            );
            super::CaptureRate::Unknown
        }
    }
}

pub struct PwAudioCapturer {
    chunks: Receiver<Vec<f32>>,
    channels: u32,
    quit: pipewire::channel::Sender<Terminate>,
    /// After every claim: the operator's output for the thread's [`host_bridge`].
    host: pipewire::channel::Sender<Option<String>>,
    sink_name: Option<String>,
    claimed: bool,
    /// Shared with the PipeWire thread so drop counting can tell "encode fell
    /// behind" from "nobody is reading". A parked capturer outlives its
    /// session after [`idle`](AudioCapturer::idle); without this, a full hand-off
    /// channel reports 100 % drop with no stream. Distinct from `claimed`.
    active: Arc<AtomicBool>,
    /// Graph-negotiated rate, written by the format callback, read by
    /// [`AudioCapturer::sample_rate`]. Seeded with the ask. In monitor mode
    /// this is the resampled stream, not the node upstream — the hi-res gate
    /// reads that from [`monitor_rate`], not here.
    negotiated_rate: Arc<AtomicU32>,
    /// Settings this capturer opened under; see [`AudioCapturer::reusable`].
    policy: OpenPolicy,
}

impl PwAudioCapturer {
    pub fn open(channels: u32, rate_hz: u32) -> Result<PwAudioCapturer> {
        Self::open_named(channels, rate_hz, None, false)
    }

    /// [`open`](Self::open) with a caller-chosen sink `node.name` so isolation
    /// can pin nested apps (`PULSE_SINK`) to the same node it captures.
    /// Must keep the `punktfunk-speaker` prefix (claim-staleness and the
    /// graph-driver diagnostic match on it). Ignored in monitor mode.
    ///
    /// `tap` taps that sink's monitor without minting it: the sink belongs to
    /// the session this one joined. The tap still claims it, so the owner's
    /// release cannot restore the default while this session reads it.
    pub fn open_named(
        channels: u32,
        rate_hz: u32,
        sink_override: Option<&str>,
        tap: bool,
    ) -> Result<PwAudioCapturer> {
        anyhow::ensure!(
            matches!(channels, 1 | 2 | 6 | 8),
            "unsupported audio channel count {channels} (want 2, 6 or 8)"
        );
        anyhow::ensure!(rate_hz > 0, "audio capture rate must be positive");
        let target = sink_override.filter(|_| tap).map(str::to_string);
        let policy = OpenPolicy::now();
        let mode = if target.is_some() {
            CaptureMode::Monitor
        } else {
            policy.mode
        };
        // Unique per capturer: overlapping instances must not alias, and a
        // fresh name gets unity WirePlumber volume, not the previous run's.
        // One sequence for both names so tap and sink log as one capturer.
        let seq = {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            SEQ.fetch_add(1, Ordering::Relaxed)
        };
        let pid = std::process::id();
        let sink_node = sink_override
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}-{pid}-{seq}", stream_sink::SINK_NAME_PREFIX));
        let nodes = CaptureNodes {
            mode,
            // StreamSink: the capture stream IS the sink, so it wears that name.
            // Otherwise a tap name that cannot match the speaker-prefix
            // crash-staleness rule or the graph-driver diagnostic.
            capture: match mode {
                CaptureMode::StreamSink => sink_node.clone(),
                _ => format!("punktfunk-audio-{pid}-{seq}"),
            },
            sink: mode.owns_sink().then_some(sink_node),
            target,
        };
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        let (host_tx, host_rx) = pipewire::channel::channel::<Option<String>>();
        let sink_name = nodes.sink.clone().or_else(|| nodes.target.clone());
        // Opens at session start, so the consumer is live from the first chunk.
        let active = Arc::new(AtomicBool::new(true));
        let thread_active = Arc::clone(&active);
        let negotiated_rate = Arc::new(AtomicU32::new(rate_hz));
        let thread_rate = Arc::clone(&negotiated_rate);
        // Stream-sink: the sink node exists before the default is claimed below. The thread
        // keeps no handle: it exits on Terminate.
        crate::ready::spawn_ready(
            "punktfunk-pw-audio",
            Duration::from_secs(5),
            move |ready| {
                if let Err(e) = pw_thread(
                    tx,
                    quit_rx,
                    host_rx,
                    channels,
                    rate_hz,
                    nodes,
                    ready,
                    thread_active,
                    thread_rate,
                ) {
                    tracing::error!(error = %format!("{e:#}"), "pipewire audio thread failed");
                }
            },
            |_detached| {
                // It may still come up; it must not outlive this error with a live sink.
                let _ = quit_tx.send(Terminate);
                anyhow!("pipewire audio init timed out")
            },
        )?;
        // Routing claim starts with the session; release is `idle()` or Drop.
        let claimed = match &sink_name {
            Some(name) => {
                stream_sink::claim(name);
                let _ = host_tx.send(stream_sink::host_sink());
                true
            }
            None => false,
        };
        Ok(PwAudioCapturer {
            chunks: rx,
            channels,
            quit: quit_tx,
            host: host_tx,
            sink_name,
            claimed,
            active,
            negotiated_rate,
            policy,
        })
    }
}

impl Drop for PwAudioCapturer {
    fn drop(&mut self) {
        // Receiver dies with us; remaining producer pushes must not count as
        // encode-thread-behind.
        self.active.store(false, Ordering::Relaxed);
        if let (true, Some(name)) = (self.claimed, &self.sink_name) {
            self.claimed = false;
            stream_sink::release(name);
        }
        // Failed send means the thread already exited — nothing to tear down.
        let _ = self.quit.send(Terminate);
    }
}

impl AudioCapturer for PwAudioCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        self.next_chunk_within(Duration::from_secs(5))
    }

    fn next_chunk_within(&mut self, budget: Duration) -> Result<Vec<f32>> {
        match self.chunks.recv_timeout(budget) {
            Ok(c) => Ok(c),
            // Quiet sink is not a failure — empty chunk keeps the capturer alive.
            // Only a dead capture thread is Err (caller reopens).
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("pipewire audio thread ended")),
        }
    }

    fn channels(&self) -> u32 {
        self.channels
    }

    fn sink_name(&self) -> Option<&str> {
        self.sink_name.as_deref()
    }

    fn sample_rate(&self) -> u32 {
        self.negotiated_rate.load(Ordering::Relaxed)
    }

    fn reusable(&self) -> bool {
        self.policy == OpenPolicy::now()
    }

    fn drain(&mut self) {
        while self.chunks.try_recv().is_ok() {}
        // After the backlog drain, so the producer never counts a drop against
        // a channel this call is still emptying.
        self.active.store(true, Ordering::Relaxed);
    }

    fn idle(&mut self) {
        // Parked: channel fills and stays full; those drops are nobody's fault.
        self.active.store(false, Ordering::Relaxed);
        if let (true, Some(name)) = (self.claimed, &self.sink_name) {
            self.claimed = false;
            stream_sink::release(name);
        }
        // No session to route for: pins and playthrough links come down.
        let _ = self.host.send(None);
    }
}

/// [`spa_channel_order`] as `audio.position` (`"[ FL FR ]"`) — the null-sink
/// adapter is configured by properties, not a format pod. [`spa_positions`] is
/// the pod view of the same list.
fn spa_position_names(channels: u32) -> String {
    let names: Vec<&str> = spa_channel_order(channels as u8)
        .iter()
        .map(|(_, n)| *n)
        .collect();
    format!("[ {} ]", names.join(" "))
}

/// Property set of the host-owned `support.null-audio-sink`.
///
/// Returns `(key, value)` pairs so the tests below can pin each invariant:
///
/// * `factory.name` + no `object.linger`: pipewire-pulse `module-null-sink`;
///   lifetime is this connection — a crash leaves no ghost sink.
/// * `audio.rate`/`channels`/`position`: session format, making
///   [`probe_capture_rate`]'s `Declared` honest.
/// * **`node.force-quantum`, not `node.latency`**: a driver's quantum is the
///   smallest follower `node.latency`, then rounded down to a power of two.
///   A 240-frame (5 ms) ask is served as 128 on stock Linux. This key skips
///   that rounding and, driving only its own group, forces nothing else.
/// * `priority.session = 50`: an unclaimed sink must not win automatic default
///   election; routing is the [`stream_sink`] claim.
/// * **No `priority.driver`**: never elected to clock someone else's group.
/// * `session.suspend-timeout-seconds = 0`: suspend/resume is a hole in a
///   live stream (Wine churns its device at launch).
/// * `monitor.*`: pipewire-pulse defaults, so the volume slider still works.
fn null_sink_props(name: &str, channels: u32, rate_hz: u32) -> Vec<(&'static str, String)> {
    vec![
        ("factory.name", "support.null-audio-sink".to_string()),
        ("node.name", name.to_string()),
        ("node.description", "Punktfunk Stream Speaker".to_string()),
        ("media.class", "Audio/Sink".to_string()),
        ("node.virtual", "true".to_string()),
        ("audio.rate", rate_hz.to_string()),
        ("audio.channels", channels.to_string()),
        ("audio.position", spa_position_names(channels)),
        ("priority.session", "50".to_string()),
        ("session.suspend-timeout-seconds", "0".to_string()),
        (
            "node.force-quantum",
            capture_quantum_frames(rate_hz).to_string(),
        ),
        ("monitor.channel-volumes", "true".to_string()),
        ("monitor.passthrough", "true".to_string()),
    ]
}

/// Graph quantum every punktfunk PipeWire stream asks for, in frames:
/// 240 @ 48 kHz = 5 ms, one protocol audio frame. Named so the `NODE_LATENCY`
/// ask and the honoured-or-not check cannot drift.
const CAPTURE_QUANTUM_FRAMES: u32 = 240;

/// [`CAPTURE_QUANTUM_FRAMES`] at `rate_hz` — the same 5 ms of wall time.
/// A 96 kHz session (`design/hi-res-audio.md`) asking for a flat 240 frames
/// would halve the quantum to 2.5 ms. Virtual mic and pad sinks stay 48 kHz
/// and keep the constant.
fn capture_quantum_frames(rate_hz: u32) -> u32 {
    // `max(1)` guards a nonsense rate from a zero-frame ask.
    ((CAPTURE_QUANTUM_FRAMES as u64 * rate_hz as u64 / SAMPLE_RATE as u64) as u32).max(1)
}

/// Consecutive callbacks that must agree on a new buffer size before it
/// replaces the one gaps are scored against. Three rejects a boundary
/// artefact and still adopts a genuine re-plan within ~15 ms.
const QUANTUM_CONFIRM: u8 = 3;

/// Confirms a new capture quantum only once it holds for [`QUANTUM_CONFIRM`] callbacks, so
/// one short buffer can't move the gap threshold. Per open: a host runs for days, and a
/// process-wide latch reported the first capture then never again.
#[derive(Default)]
struct QuantumTracker {
    /// Frames per callback, `0` until confirmed.
    frames: usize,
    /// A size seen but not yet believed, with its consecutive count.
    candidate: Option<(usize, u8)>,
}

impl QuantumTracker {
    /// One callback of `frames`. `Some(was)` when `frames` is newly confirmed; `was` is `0`
    /// for the first.
    fn observe(&mut self, frames: usize) -> Option<usize> {
        if frames > 0 && frames != self.frames {
            let streak = match self.candidate {
                Some((f, c)) if f == frames => c.saturating_add(1),
                _ => 1,
            };
            if streak < QUANTUM_CONFIRM {
                self.candidate = Some((frames, streak));
                return None;
            }
            self.candidate = None;
            return Some(std::mem::replace(&mut self.frames, frames));
        }
        if frames == self.frames {
            self.candidate = None;
        }
        None
    }
}

/// The capture stream's state, owned by its PipeWire listener.
struct CapUd {
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    channels: u32,
    stats: crate::capture_policy::CaptureStats,
    last_stats: std::time::Instant,
    quantum_frames: QuantumTracker,
    reported_sched: bool,
    /// Last callback time, so cadence can be scored. Cleared across a state transition — a
    /// deliberate Paused span is not one hole. Not in `stats`: stats reset every window.
    last_cb: Option<std::time::Instant>,
    /// Negotiated quantum; a gap is measured against this. Seeded with the ask; corrected
    /// on first data. A 1024-frame clamp is the deal we got, not a fault.
    quantum: Duration,
    /// Current format, so a resume to the same one is not a real change.
    negotiated: Option<(pipewire::spa::param::audio::AudioFormat, u32, u32)>,
    active: Arc<AtomicBool>,
    /// When the stream last left `Streaming`, so the span is charged to the window it
    /// stretched. `None` while streaming.
    paused_since: Option<std::time::Instant>,
    /// Denominator for every frames↔time conversion. At 96 kHz a hardcoded 48 000 would
    /// report every quantum as twice as long.
    rate_hz: u32,
    negotiated_rate: Arc<AtomicU32>,
}

impl CapUd {
    fn new(
        tx: std::sync::mpsc::SyncSender<Vec<f32>>,
        channels: u32,
        rate_hz: u32,
        active: Arc<AtomicBool>,
        negotiated_rate: Arc<AtomicU32>,
    ) -> CapUd {
        CapUd {
            tx,
            channels,
            stats: Default::default(),
            last_stats: std::time::Instant::now(),
            quantum_frames: QuantumTracker::default(),
            reported_sched: false,
            last_cb: None,
            quantum: Duration::from_micros(
                capture_quantum_frames(rate_hz) as u64 * 1_000_000 / rate_hz as u64,
            ),
            negotiated: None,
            active,
            paused_since: None,
            rate_hz,
            negotiated_rate,
        }
    }

    /// A state change. A Paused↔Streaming span is a gap in the stream existing, not in
    /// delivery: scoring it would bury the sub-10 ms holes. It is still reported, charged
    /// to the window flushed after the resume, since that window stretches by the span.
    fn on_state(&mut self, streaming: bool) {
        self.last_cb = None;
        if streaming {
            if let Some(since) = self.paused_since.take() {
                self.stats.observe_pause(since.elapsed());
            }
        } else {
            self.paused_since
                .get_or_insert_with(std::time::Instant::now);
        }
    }

    /// A negotiated `(format, rate, channels)`. The same one again is the graph resuming us,
    /// not a stream change. What is reported is what was granted, not asked
    /// (`design/hi-res-audio.md`).
    fn on_format(
        &mut self,
        now: (pipewire::spa::param::audio::AudioFormat, u32, u32),
        mode: CaptureMode,
    ) {
        if self.negotiated == Some(now) {
            tracing::debug!(
                format = ?now.0,
                rate = now.1,
                channels = now.2,
                "audio format renegotiated, unchanged (the graph resumed our sink)"
            );
            return;
        }
        self.negotiated = Some(now);
        // Rate `0` means the pod carried none: "unstated" is not a claim that it changed.
        if now.1 != 0 {
            self.rate_hz = now.1;
            self.negotiated_rate.store(now.1, Ordering::Relaxed);
        }
        // Sink modes: we own the sink, so this IS the format apps render into. Monitor mode:
        // PipeWire's resampler reports a clean rate whatever ran upstream; the node's own
        // rate is a registry lookup in `monitor_rate`.
        tracing::info!(
            format = ?now.0,
            rate = now.1,
            channels = now.2,
            mode = mode.as_str(),
            "audio format negotiated"
        );
    }

    /// Score a callback's arrival, before any early return: a callback that ran empty still
    /// ran, which is different from one that never ran.
    fn on_callback(&mut self, now: std::time::Instant) {
        let since_last = self.last_cb.map(|t| now.duration_since(t));
        self.last_cb = Some(now);
        self.stats.observe_callback(since_last, self.quantum);
        if !self.reported_sched {
            self.reported_sched = true;
            // The thread that actually runs this callback, once per open. The mainloop
            // boost never reaches here.
            let (policy, rt_priority, nice) = pf_frame::thread_qos::current_thread_sched();
            tracing::info!(
                policy,
                rt_priority,
                nice,
                "audio capture callback scheduling"
            );
        }
    }

    /// One dequeued buffer, negotiated as F32LE interleaved. The graph re-plans its quantum
    /// when anything else asks for a different latency, so the size is tracked, not latched.
    fn on_region(&mut self, region: &[u8]) {
        let frames = region.len() / 4 / (self.channels.max(1) as usize);
        if let Some(was) = self.quantum_frames.observe(frames) {
            self.note_quantum(was, frames);
        }
        let samples: Vec<f32> = region
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        self.stats.observe(&samples, self.channels);
        // Lossy and non-blocking. Count only while a session is reading: a full channel
        // under a live consumer is a click plus a permanent shift; under a parked capturer
        // it is nothing.
        if self.tx.try_send(samples).is_err() && self.active.load(Ordering::Relaxed) {
            self.stats.dropped_chunks += 1;
        }
        self.stats
            .flush_window(&mut self.last_stats, self.rate_hz, None);
    }

    fn note_quantum(&mut self, was: usize, frames: usize) {
        let rate = self.rate_hz.max(1);
        self.quantum = Duration::from_micros(frames as u64 * 1_000_000 / rate as u64);
        let want = capture_quantum_frames(self.rate_hz) as usize;
        let negotiated_ms = format!("{:.1}", frames as f32 * 1000.0 / rate as f32);
        if was != 0 {
            // Moves the gap threshold under a reader comparing windows.
            tracing::info!(
                previous_frames = was,
                negotiated_frames = frames,
                negotiated_ms,
                "the audio graph re-planned our quantum mid-stream"
            );
        } else if frames > want {
            // Asked vs granted. Stock `pipewire.conf` raises `default.clock.min-quantum` to
            // 1024 in a VM (`cpu.vm.name` set), so a 5 ms ask becomes 21.3 ms.
            tracing::warn!(
                requested_frames = want,
                negotiated_frames = frames,
                negotiated_ms,
                "the audio graph refused our low-latency quantum — capture \
                 arrives in bursts this size, and the client must buffer at \
                 least that much to play them smoothly. On a VM this is \
                 PipeWire's `default.clock.min-quantum = 1024` rule; check \
                 `pw-metadata -n settings`"
            );
        } else {
            tracing::info!(
                requested_frames = want,
                negotiated_frames = frames,
                "audio capture quantum negotiated"
            );
        }
    }
}

/// The null-sink mode's `support.null-audio-sink` adapter. The server answers
/// asynchronously: `bound` is the sink existing, `error` is no adapter factory. The
/// core-error listener ends the thread either way.
fn create_null_sink(
    core: &pipewire::core::CoreRc,
    name: &str,
    channels: u32,
    rate_hz: u32,
) -> Result<(pipewire::node::Node, pipewire::proxy::ProxyListener)> {
    use pipewire::proxy::ProxyT;
    let mut props = pipewire::properties::PropertiesBox::new();
    for (key, value) in null_sink_props(name, channels, rate_hz) {
        props.insert(key, value);
    }
    let node = core
        .create_object::<pipewire::node::Node>("adapter", &props)
        .context("create the punktfunk stream sink (support.null-audio-sink)")?;
    let listener = node
        .upcast_ref()
        .add_listener_local()
        .bound(|id| {
            tracing::debug!(node_id = id, "punktfunk stream sink registered");
        })
        .error(|_seq, res, message| {
            tracing::warn!(
                res,
                message,
                "the punktfunk stream sink was not created — no desktop audio \
                 capture until it is. Set PUNKTFUNK_STREAM_SINK=stream for the 0.30 \
                 topology (no created sink)"
            );
        })
        .register();
    Ok((node, listener))
}

/// `node.driver-id` names who clocks our group. Not in the registry announce set — needs a
/// bind + `info`. The daemon writes the key but flushes on the next info emission, so this
/// is last-known, not realtime.
struct GraphDriver {
    /// Our node, bound so its `info` — and `node.driver-id` — arrives.
    ours: Option<(pipewire::node::Node, pipewire::node::NodeListener)>,
    /// Last reported; log only on change.
    driver: Option<u32>,
}

/// Name the node clocking our capture group. `expected` is our own sink in null-sink mode,
/// the one right answer until playthrough links the host output; legacy topologies borrow
/// a driver by design, so the line names it without judging.
fn report_graph_driver(bridge: &host_bridge::HostBridge, expected: Option<&str>, id: u32) {
    let driver = bridge.node_name(id).unwrap_or("<unnamed>");
    match expected {
        Some(sink) if driver == sink => {
            tracing::info!(driver, driver_id = id, "audio capture graph driver")
        }
        Some(_) if bridge.is_host(driver) => tracing::info!(
            driver,
            driver_id = id,
            "audio capture graph driver (host playthrough — the \
             host output clocks the group)"
        ),
        Some(sink) => tracing::warn!(
            driver,
            driver_id = id,
            expected = sink,
            "our audio capture group is being clocked by another \
             node — every hole in this stream is that node's \
             scheduling, not ours. Something has linked our sink to \
             it (a loopback from its monitor is the usual cause); a \
             USB or USB-over-IP sound card here is the 2026-08-18 \
             defect"
        ),
        None => tracing::info!(
            driver,
            driver_id = id,
            "audio capture graph driver (borrowed — this topology \
             has none of its own)"
        ),
    }
}

/// The capture stream's properties per [`CaptureMode`]. `node_latency` is
/// `<quantum frames>/<rate>`, one string for every arm so they can't drift.
fn capture_props(
    mode: CaptureMode,
    sink: Option<&str>,
    capture: &str,
    target: Option<&str>,
    node_latency: &str,
) -> Result<pipewire::properties::PropertiesBox> {
    use pipewire as pw;
    use pw::properties::properties;
    let mut p = match mode {
        // Monitor tap of the null sink, aimed by name so it can only be ours.
        CaptureMode::NullSink => {
            let name = sink.context("null-sink mode without a sink name")?;
            let mut p = properties! {
                *pw::keys::MEDIA_TYPE          => "Audio",
                *pw::keys::MEDIA_CATEGORY      => "Capture",
                *pw::keys::MEDIA_ROLE          => "Music",
                *pw::keys::STREAM_CAPTURE_SINK => "true",
                // A passive link does not make either end runnable. Parked,
                // nothing playing: the group is idle and the timer parks.
                // A game's (non-passive) link makes the sink runnable and
                // the graph walks that through the monitor to us.
                *pw::keys::NODE_PASSIVE        => "true",
                // Never fall back to a hardware monitor (wrong audio, and
                // briefly rejoining a hardware driver is this mode's defect).
                // These two are a pair: WirePlumber reads `dont-fallback`
                // alone as licence to destroy the stream; `linger` waits.
                "node.dont-fallback"           => "true",
                "node.linger"                  => "true",
            };
            p.insert(*pw::keys::NODE_NAME, capture);
            // Spelled out: pipewire-rs exposes `TARGET_OBJECT` only behind
            // `v0_3_44`. WirePlumber matches this against `node.name`.
            p.insert("target.object", name);
            p
        }
        // This stream IS the sink. Apps play into it; process() gets the mix.
        CaptureMode::StreamSink => {
            let name = sink.context("stream-sink mode without a sink name")?;
            let mut p = properties! {
                *pw::keys::MEDIA_TYPE       => "Audio",
                *pw::keys::MEDIA_CLASS      => "Audio/Sink",
                *pw::keys::NODE_DESCRIPTION => "Punktfunk Stream Speaker",
                *pw::keys::NODE_VIRTUAL     => "true",
                // Low on purpose, like the mic. Parked sink must not win
                // auto default election; routing is the claim.
                "priority.session"          => "50",
                // Wine churns its device at launch; each suspend/resume is a
                // hole in a live stream. Not `node.always-process`: that
                // would keep the callback scheduled ~200/s between sessions.
                "session.suspend-timeout-seconds" => "0",
            };
            p.insert(*pw::keys::NODE_NAME, name);
            p
        }
        // Default-sink monitor (system output), not a microphone, unless a
        // `target` names another session's sink.
        CaptureMode::Monitor => {
            let mut p = properties! {
                *pw::keys::MEDIA_TYPE          => "Audio",
                *pw::keys::MEDIA_CATEGORY      => "Capture",
                *pw::keys::MEDIA_ROLE          => "Music",
                *pw::keys::STREAM_CAPTURE_SINK => "true",
            };
            p.insert(*pw::keys::NODE_NAME, capture);
            if let Some(t) = target {
                p.insert("target.object", t);
            }
            p
        }
    };
    // ~5 ms quantum, one protocol frame, at the session rate.
    p.insert(*pw::keys::NODE_LATENCY, node_latency);
    Ok(p)
}

/// The desktop-audio capture thread: the PipeWire mainloop that wires the sink, the
/// graph-driver watch, the host bridge and the capture stream, then runs until quit.
/// Setup errors reach the opener through `ready`.
#[allow(clippy::too_many_arguments)]
fn pw_thread(
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    host_rx: pipewire::channel::Receiver<Option<String>>,
    channels: u32,
    rate_hz: u32,
    nodes: CaptureNodes,
    ready: std::sync::mpsc::SyncSender<Result<()>>,
    active: Arc<AtomicBool>,
    negotiated_rate: Arc<AtomicU32>,
) -> Result<()> {
    use pipewire as pw;
    use pw::spa;
    use spa::param::audio::AudioInfoRaw;
    use spa::pod::Pod;
    use std::cell::RefCell;
    use std::rc::Rc;
    let CaptureNodes {
        mode,
        sink: sink_name,
        capture: capture_name,
        target,
    } = nodes;
    // Boosts THIS mainloop thread, not the capture callback. `RT_PROCESS`
    // runs `process()` on libpipewire's data loop (`SCHED_RR`). Kept: this
    // thread still dispatches state/format, and IS the capture thread in
    // monitor mode. The callback reports its own scheduling on first entry.
    pf_frame::thread_qos::boost_thread_priority(true);

    // Setup errors funnel through the ready handshake.
    let result = (|| -> Result<()> {
        let (mainloop, core) = pw_setup::pw_connect("pw audio")?;

        // What the operator hears: playthrough links and voice-chat pins once
        // the claim reports the host output. Also names nodes for the driver line.
        let bridge = Rc::new(RefCell::new(host_bridge::HostBridge::new(
            sink_name.as_deref().unwrap_or(""),
            mode == CaptureMode::NullSink,
            mode.owns_sink(),
        )));
        // Quit flushes the bridge's undo first: a pin left behind would keep a
        // voice app off the default after the session.
        let quit_seq: Rc<RefCell<Option<spa::utils::result::AsyncSeq>>> =
            Rc::new(RefCell::new(None));
        let _quit_guard = quit_rx.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            let bridge = bridge.clone();
            let core = core.clone();
            let quit_seq = quit_seq.clone();
            move |_| {
                if bridge.borrow_mut().clear(&core) {
                    if let Ok(seq) = core.sync(0) {
                        *quit_seq.borrow_mut() = Some(seq);
                        return;
                    }
                }
                mainloop.quit();
            }
        });
        let _host_guard = host_rx.attach(mainloop.loop_(), {
            let bridge = bridge.clone();
            let core = core.clone();
            move |host| {
                let mut b = bridge.borrow_mut();
                // Parked (`idle`): nothing to route to until the next claim.
                if host.is_none() {
                    b.clear(&core);
                }
                b.set_host(&core, host);
                b.sync(&core);
            }
        });

        // Core error ends this thread so the chunk channel disconnects and
        // `next_chunk` returns Err (reopen-with-backoff). Without this, a
        // restart left `next_chunk` returning quiet-sink empties forever.
        let _core_listener = core
            .add_listener_local()
            .done({
                let mainloop = mainloop.clone();
                let quit_seq = quit_seq.clone();
                move |id, seq| {
                    if id == pw::core::PW_ID_CORE && *quit_seq.borrow() == Some(seq) {
                        mainloop.quit();
                    }
                }
            })
            .error({
                let mainloop = mainloop.clone();
                let bridge = bridge.clone();
                move |id, _seq, res, message| {
                    // A refused playthrough link is that link's failure, not the capture's.
                    if bridge.try_borrow().is_ok_and(|b| b.owns_proxy(id)) {
                        tracing::warn!(id, res, message, "host bridge object refused");
                        return;
                    }
                    tracing::warn!(id, res, message, "pipewire core error — audio capture ends");
                    mainloop.quit();
                }
            })
            .register();

        // Null-sink mode: a real `support.null-audio-sink` adapter (a driver
        // with its own `timerfd`). Created before the capture stream so
        // `target.object` resolves; no `object.linger`, so it dies with this
        // connection — which the loop thread's exit relies on.
        let _sink_node = match mode {
            CaptureMode::NullSink => Some(create_null_sink(
                &core,
                sink_name
                    .as_deref()
                    .context("null-sink mode without a sink name")?,
                channels,
                rate_hz,
            )?),
            _ => None,
        };
        // `<quantum frames>/<rate>` — both halves move so the ask stays 5 ms
        // at 48 kHz and 96 kHz. Formatted once so the property arms cannot drift.
        let node_latency = format!("{}/{}", capture_quantum_frames(rate_hz), rate_hz);
        // Null-sink has exactly one right answer (ours), or the host output
        // once playthrough links it. Legacy topologies borrow a driver by
        // design, so the line names it without judging.
        let expected_driver = match mode {
            CaptureMode::NullSink => sink_name.clone(),
            _ => None,
        };
        let watch = Rc::new(RefCell::new(GraphDriver {
            ours: None,
            driver: None,
        }));
        let registry = core.get_registry_rc().context("pw audio registry")?;
        let _registry_listener = registry
            .add_listener_local()
            .global({
                let watch = watch.clone();
                let bridge = bridge.clone();
                let core = core.clone();
                let registry = registry.clone();
                let capture_name = capture_name.clone();
                move |global| {
                    {
                        let mut b = bridge.borrow_mut();
                        b.on_global(global, &registry);
                        b.sync(&core);
                    }
                    if global.type_ != pw::types::ObjectType::Node {
                        return;
                    }
                    let Some(props) = global.props else { return };
                    let Some(name) = props.get("node.name") else {
                        return;
                    };
                    if name != capture_name.as_str() || watch.borrow().ours.is_some() {
                        return;
                    }
                    let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
                        return;
                    };
                    let listener = node
                        .add_listener_local()
                        .info({
                            let watch = watch.clone();
                            let bridge = bridge.clone();
                            let expected = expected_driver.clone();
                            move |info| {
                                let Some(props) = info.props() else { return };
                                // Absent = between drivers (daemon drops the key).
                                // The next assignment reports itself.
                                let Some(id) = props
                                    .get("node.driver-id")
                                    .and_then(|v| v.parse::<u32>().ok())
                                else {
                                    return;
                                };
                                let mut w = watch.borrow_mut();
                                if w.driver == Some(id) {
                                    return;
                                }
                                w.driver = Some(id);
                                report_graph_driver(&bridge.borrow(), expected.as_deref(), id);
                            }
                        })
                        .register();
                    watch.borrow_mut().ours = Some((node, listener));
                }
            })
            .global_remove({
                let bridge = bridge.clone();
                move |id| {
                    bridge.borrow_mut().on_remove(id);
                }
            })
            .register();

        let props = capture_props(
            mode,
            sink_name.as_deref(),
            &capture_name,
            target.as_deref(),
            &node_latency,
        )?;
        let stream = pw::stream::StreamBox::new(&core, "punktfunk-audio", props)
            .context("pw audio Stream")?;

        let ud = CapUd::new(tx, channels, rate_hz, active, negotiated_rate);
        let _listener = stream
            .add_local_listener_with_user_data(ud)
            .state_changed({
                let mainloop = mainloop.clone();
                move |_s, ud, old, new| {
                    tracing::debug!(?old, ?new, "pipewire audio stream state");
                    ud.on_state(matches!(new, pw::stream::StreamState::Streaming));
                    // Unrecoverable — exit so sessions reopen a fresh instance.
                    if matches!(new, pw::stream::StreamState::Error(_)) {
                        mainloop.quit();
                    }
                }
            })
            .param_changed(move |_stream, ud, id, param| {
                let Some(param) = param else { return };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = AudioInfoRaw::default();
                if info.parse(param).is_ok() {
                    ud.on_format((info.format(), info.rate(), info.channels()), mode);
                }
            })
            .process(|stream, ud| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    ud.on_callback(std::time::Instant::now());
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        ud.stats.missed_dequeues += 1;
                        return;
                    };
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        ud.stats.missed_dequeues += 1;
                        return;
                    }
                    let d = &mut datas[0];
                    let (offset, size) = {
                        let c = d.chunk();
                        (c.offset() as usize, c.size() as usize)
                    };
                    let Some(buf) = d.data() else {
                        ud.stats.missed_dequeues += 1;
                        return;
                    };
                    if offset > buf.len() {
                        ud.stats.missed_dequeues += 1;
                        return;
                    }
                    ud.on_region(&buf[offset..(offset + size).min(buf.len())]);
                }));
                if outcome.is_err() {
                    tracing::error!("panic in pipewire audio callback — chunk dropped");
                }
            })
            .register()
            .context("register audio stream listener")?;

        // Sink modes: the sink's advertised layout. Monitor mode: PipeWire's mixer remixes the
        // monitor and resamples the rate, so hi-res is proven in `monitor_rate`, not here.
        let values = pw_setup::f32_format_pod(rate_hz, channels, spa_positions(channels as u8))?;
        let mut params = [Pod::from_bytes(&values).context("audio pod from bytes")?];

        // Same reason as the mic: a synchronous node that joins its driver
        // group. Also puts the callback on a SCHED_RR data loop the mainloop
        // boost above can never reach.
        let mut flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
        if mode.owns_sink() {
            flags |= pw::stream::StreamFlags::RT_PROCESS;
        }
        stream
            .connect(
                spa::utils::Direction::Input,
                None, // PW_ID_ANY — legacy: the default sink monitor
                flags,
                &mut params,
            )
            .context("pw audio stream connect")?;

        // Connect is async server-side; if the default-sink claim lands before
        // the node registers, WirePlumber keeps the configured value and elects
        // it when the node appears.
        tracing::info!(
            mode = mode.as_str(),
            sink = sink_name.as_deref().unwrap_or("<the default sink>"),
            capture = capture_name.as_str(),
            "desktop audio capture topology"
        );
        let _ = ready.send(Ok(()));
        mainloop.run();
        tracing::debug!("pipewire audio loop exited (capturer dropped)");
        Ok(())
    })();
    if let Err(e) = &result {
        let _ = ready.send(Err(anyhow!("{e:#}")));
    }
    result
}

// ---- the platform seam `lib.rs` calls through -----------------------------------------------

/// Open a live capturer for system output. Default: host-owned stream sink claimed as
/// the default, advertising `channels` so apps can produce real surround.
/// `PUNKTFUNK_STREAM_SINK=0`: default-sink monitor, missing positions filled with
/// silence. `rate_hz` is a request; the grant is [`AudioCapturer::sample_rate`].
pub(super) fn open_audio_capture(channels: u32, rate_hz: u32) -> Result<Box<dyn AudioCapturer>> {
    PwAudioCapturer::open(channels, rate_hz).map(|c| Box::new(c) as Box<dyn AudioCapturer>)
}

/// [`open_audio_capture`] pinned to a sink `node.name` (`design/gamescope-multiuser.md`):
/// gamescope apps get `PULSE_SINK` and we capture that sink's monitor. `None` =
/// [`open_audio_capture`]. `tap`: the sink is another session's.
pub(super) fn open_audio_capture_named(
    channels: u32,
    rate_hz: u32,
    sink: Option<&str>,
    tap: bool,
) -> Result<Box<dyn AudioCapturer>> {
    PwAudioCapturer::open_named(channels, rate_hz, sink, tap)
        .map(|c| Box::new(c) as Box<dyn AudioCapturer>)
}

/// Stream/null-sink mode can mint a per-session sink; monitor mode shares the default output.
pub(super) fn per_session_sink_possible() -> bool {
    sink_capture_active()
}

/// PipeWire `Audio/Source`, the shared `punktfunk-mic`.
pub(super) fn open_virtual_mic(channels: u32) -> Result<Box<dyn VirtualMic>> {
    open_virtual_mic_named(channels, None)
}

/// [`open_virtual_mic`] pinned to a source `node.name` (`design/gamescope-multiuser.md`:
/// `punktfunk-mic-{id}`, gamescope `PULSE_SOURCE`). `None` = shared `punktfunk-mic`.
pub(super) fn open_virtual_mic_named(
    channels: u32,
    source: Option<&str>,
) -> Result<Box<dyn VirtualMic>> {
    mic::PwMicSource::open_named(channels, source).map(|m| Box::new(m) as Box<dyn VirtualMic>)
}

/// No wiring pass on Linux.
pub(super) fn wiring_snapshot() -> Option<super::wiring_plan::Wiring> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three spellings of `PUNKTFUNK_STREAM_SINK`; anything else is the default
    /// so a typo in a debug lever cannot be why a session has no audio.
    #[test]
    fn capture_mode_grammar() {
        assert_eq!(capture_mode_from(None), CaptureMode::NullSink);
        for off in ["0", "false", "no", "off", " off "] {
            assert_eq!(
                capture_mode_from(Some(off)),
                CaptureMode::Monitor,
                "{off:?} selects the legacy monitor follower"
            );
        }
        assert_eq!(capture_mode_from(Some("stream")), CaptureMode::StreamSink);
        assert_eq!(capture_mode_from(Some(" stream ")), CaptureMode::StreamSink);
        for junk in ["1", "yes", "null", "STREAM", ""] {
            assert_eq!(
                capture_mode_from(Some(junk)),
                CaptureMode::NullSink,
                "{junk:?} is not a mode and must fall to the default"
            );
        }
        assert!(CaptureMode::NullSink.owns_sink() && CaptureMode::StreamSink.owns_sink());
        assert!(!CaptureMode::Monitor.owns_sink());
    }

    /// These exact strings are what PipeWire parses.
    #[test]
    fn channel_map_names_parse() {
        assert_eq!(spa_position_names(1), "[ MONO ]");
        assert_eq!(spa_position_names(2), "[ FL FR ]");
        assert_eq!(spa_position_names(6), "[ FL FR FC LFE RL RR ]");
        assert_eq!(spa_position_names(8), "[ FL FR FC LFE RL RR SL SR ]");
    }

    /// A new buffer size moves the gap threshold only after `QUANTUM_CONFIRM` callbacks in a
    /// row; the confirmed size coming back resets a candidate.
    #[test]
    fn a_new_quantum_needs_three_callbacks_in_a_row() {
        let mut q = QuantumTracker::default();
        assert_eq!(q.observe(0), None, "an empty buffer is not a size");
        assert_eq!((q.observe(240), q.observe(240)), (None, None));
        assert_eq!(q.observe(240), Some(0), "first confirmation");
        assert_eq!(q.observe(1024), None);
        assert_eq!(q.observe(240), None, "the old size is back");
        assert_eq!((q.observe(1024), q.observe(1024)), (None, None));
        assert_eq!(q.observe(1024), Some(240), "re-planned");
    }

    /// Each mode's stream properties, read back from the dict PipeWire gets.
    #[test]
    fn capture_props_per_mode() {
        pipewire::init();
        let get = |mode, sink, target| {
            let p = capture_props(mode, sink, "punktfunk-capture-1", target, "240/48000").unwrap();
            let keys = [
                "node.name",
                "target.object",
                "node.latency",
                "node.passive",
                "media.class",
            ];
            keys.map(|k| p.get(k).map(str::to_owned))
        };
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(
            get(CaptureMode::NullSink, Some("punktfunk-speaker-1"), None),
            [
                s("punktfunk-capture-1"),
                s("punktfunk-speaker-1"),
                s("240/48000"),
                s("true"),
                None
            ]
        );
        assert_eq!(
            get(CaptureMode::StreamSink, Some("punktfunk-speaker-1"), None),
            [
                s("punktfunk-speaker-1"),
                None,
                s("240/48000"),
                None,
                s("Audio/Sink")
            ]
        );
        assert_eq!(
            get(CaptureMode::Monitor, None, Some("punktfunk-speaker-2")),
            [
                s("punktfunk-capture-1"),
                s("punktfunk-speaker-2"),
                s("240/48000"),
                None,
                None
            ]
        );
        assert!(capture_props(CaptureMode::NullSink, None, "c", None, "240/48000").is_err());
    }

    /// Created-sink invariants. None fail loudly if they silently change.
    #[test]
    fn null_sink_props_hold_their_invariants() {
        let props = null_sink_props("punktfunk-speaker-42-0", 6, 48_000);
        let get = |k: &str| {
            props
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("factory.name"), Some("support.null-audio-sink"));
        assert_eq!(get("media.class"), Some("Audio/Sink"));
        assert_eq!(get("node.name"), Some("punktfunk-speaker-42-0"));
        assert!(
            get("node.name").is_some_and(|n| n.starts_with(stream_sink::SINK_NAME_PREFIX)),
            "the claim's staleness rule matches this prefix"
        );
        assert_eq!(get("audio.channels"), Some("6"));
        assert_eq!(get("audio.rate"), Some("48000"));
        assert_eq!(get("audio.position"), Some("[ FL FR FC LFE RL RR ]"));
        // The 5 ms ask, in the one form PipeWire will not round down to 128.
        assert_eq!(get("node.force-quantum"), Some("240"));
        assert_eq!(get("session.suspend-timeout-seconds"), Some("0"));
        assert_eq!(get("priority.session"), Some("50"));
        // A linger sink wedges routing on a node nothing owns; a driver
        // priority would clock other people's driver-less groups.
        assert_eq!(get("object.linger"), None);
        assert_eq!(get("priority.driver"), None);
        // Quantum is a latency, so it scales with the rate (5 ms either way).
        let hi = null_sink_props("punktfunk-speaker-42-1", 2, 96_000);
        let hi_get = |k: &str| {
            hi.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(hi_get("audio.rate"), Some("96000"));
        assert_eq!(hi_get("node.force-quantum"), Some("480"));
    }
}
