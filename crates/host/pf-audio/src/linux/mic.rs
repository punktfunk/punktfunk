//! PipeWire virtual microphone: a stream `Audio/Source` the host pushes decoded client-mic PCM
//! into, drained by an RT process callback through a prime→hold→re-prime jitter ring. Windows
//! twin: `windows/wasapi_mic.rs`.

use super::{pw_setup, stream_sink, Terminate};
use crate::{MicBackendStats, VirtualMic, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use punktfunk_core::audio::spa_positions;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::Arc;
use std::time::Duration;

/// Virtual microphone: a PipeWire `Audio/Source` the host pushes decoded
/// client-mic PCM into. The loop thread's producer callback drains it
/// (silence on underrun). Mirrors [`PwAudioCapturer`](super::PwAudioCapturer) inverted
/// (`Direction::Output`).
///
/// A stream node, not a `support.null-audio-sink` adapter: an
/// `Audio/Source/Virtual` adapter never gets a clock (`QUANT/RATE` 0, silence)
/// and WirePlumber reroutes a feeder targeting it to the *default sink*
/// (client voice out of the speakers, into desktop capture: echo). The
/// desktop **sink** is an adapter (`null_sink_props`); that result does not
/// transfer — WirePlumber has no monitor path for `Audio/Source/Virtual`.
/// Do not switch this to the adapter recipe without re-validating both.
///
/// Loop thread exit (core/stream error) flips `alive`; `push` returns
/// `false` and the pump reopens against the new daemon ([`VirtualMic`]).
pub struct PwMicSource {
    pcm: std::sync::mpsc::SyncSender<(std::time::Instant, Vec<f32>)>,
    channels: u32,
    quit: pipewire::channel::Sender<Terminate>,
    alive: Arc<AtomicBool>,
    /// One-shot, consumed by the process callback (clears the jitter ring).
    flush: Arc<AtomicBool>,
    ring: Arc<MicRingShared>,
}

/// Atomics between [`PwMicSource`] and the RT process callback. All `Relaxed`
/// — slowly-moving target and telemetry, not synchronization.
#[derive(Default)]
struct MicRingShared {
    /// Pump-set jitter target (per-channel samples). `0` = pump never spoke
    /// → callback keeps the historical 3-quanta clamp.
    target: AtomicUsize,
    depth: AtomicUsize,
    prime: AtomicUsize,
    reprimes: AtomicU64,
    overflow: AtomicU64,
}

impl PwMicSource {
    /// A 1- or 2-channel source under a caller-chosen `node.name`, so isolation can
    /// pin nested apps (`PULSE_SOURCE`) to this session's uplink.
    /// `None` = shared `punktfunk-mic`. PipeWire 1.4 never assigns a driver to
    /// a non-default `Audio/Source` recorded by target — per-session mic needs
    /// the 1.6 daemon; on 1.4 the election losers read silence.
    pub fn open_named(channels: u32, source_name: Option<&str>) -> Result<PwMicSource> {
        anyhow::ensure!(
            matches!(channels, 1 | 2),
            "virtual mic supports 1 or 2 channels, got {channels}"
        );
        let node_name = source_name.unwrap_or(stream_sink::MIC_NAME).to_string();
        let (pcm_tx, pcm_rx) = sync_channel::<(std::time::Instant, Vec<f32>)>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        let alive = Arc::new(AtomicBool::new(true));
        let flush = Arc::new(AtomicBool::new(false));
        let ring = Arc::new(MicRingShared::default());
        let (alive_t, flush_t, ring_t) = (alive.clone(), flush.clone(), ring.clone());
        // PipeWire not running is an open error (pump backoff), not an instantly-dead
        // instance the pump would churn on. The thread keeps no handle: it exits on Terminate.
        crate::ready::spawn_ready(
            "punktfunk-pw-mic",
            Duration::from_secs(5),
            move |ready| {
                if let Err(e) = mic_pw_thread(
                    pcm_rx, quit_rx, channels, &node_name, flush_t, ring_t, ready,
                ) {
                    // Setup/open failure only (the running mainloop exits Ok).
                    // Already reported via the ready handshake.
                    tracing::debug!(error = %format!("{e:#}"), "pipewire virtual-mic setup failed — pump will back off and retry");
                }
                // Clean quit or daemon death: this instance is done; the pump reopens.
                alive_t.store(false, Ordering::Release);
            },
            |_detached| {
                // It may still come up; it must not outlive this error with a live source.
                let _ = quit_tx.send(Terminate);
                anyhow!("pipewire virtual-mic init timed out")
            },
        )?;
        Ok(PwMicSource {
            pcm: pcm_tx,
            channels,
            quit: quit_tx,
            alive,
            flush,
            ring,
        })
    }
}

impl Drop for PwMicSource {
    fn drop(&mut self) {
        let _ = self.quit.send(Terminate);
    }
}

impl VirtualMic for PwMicSource {
    fn push(&self, pcm: &[f32]) -> bool {
        if !self.alive.load(Ordering::Acquire) {
            return false;
        }
        // Timestamped so the process callback can age out chunks that sat in
        // the channel while no recorder was attached.
        match self.pcm.try_send((std::time::Instant::now(), pcm.to_vec())) {
            Ok(()) => true,
            // Behind is fine (drop the chunk); a gone receiver means the loop exited.
            Err(std::sync::mpsc::TrySendError::Full(_)) => true,
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        }
    }
    fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    fn discard(&self) {
        self.flush.store(true, Ordering::Release);
    }
    fn channels(&self) -> u32 {
        self.channels
    }
    fn set_target_depth(&self, samples_per_ch: usize) {
        self.ring.target.store(samples_per_ch, Ordering::Relaxed);
    }
    fn depth(&self) -> Option<(usize, usize)> {
        let prime = self.ring.prime.load(Ordering::Relaxed);
        // 0 = process callback has not run yet (no consumer).
        (prime > 0).then(|| (self.ring.depth.load(Ordering::Relaxed), prime))
    }
    fn take_stats(&self) -> MicBackendStats {
        MicBackendStats {
            reprimes: self.ring.reprimes.swap(0, Ordering::Relaxed),
            overflow_dropped: self.ring.overflow.swap(0, Ordering::Relaxed),
        }
    }
}

/// Incoming decoded PCM and a capped ring the process callback drains into
/// PipeWire buffers through [`MicGate`].
struct MicUserData {
    rx: Receiver<(std::time::Instant, Vec<f32>)>,
    ring: VecDeque<f32>,
    channels: usize,
    gate: MicGate,
    flush: Arc<AtomicBool>,
    shared: Arc<MicRingShared>,
    /// Last process-callback run. A long gap means the ring predates the
    /// current consumer (idle with no recorder) and must be dropped.
    last_run: Option<std::time::Instant>,
}

/// PCM older than this never reaches a recorder: channel-aged chunks and
/// ring content from before a consumer gap would otherwise burst as stale
/// audio when recording (re)starts.
const MIC_STALE: Duration = Duration::from_secs(1);

/// The mic's jitter gate: silence until the ring holds the prime depth, then drain it; only a
/// full drain re-arms the prime. Counts are samples, not frames.
#[derive(Default)]
struct MicGate {
    primed: bool,
}

/// What one [`MicGate::fill`] did.
#[derive(Debug, PartialEq)]
struct Filled {
    /// Oldest samples dropped to bound latency.
    dropped: usize,
    /// The ring ran dry and the gate re-armed its prime.
    reprimed: bool,
}

impl MicGate {
    /// Prime depth for a `want`-sample quantum: one quantum plus the pump's jitter target
    /// (per channel). A target of `0` (no estimate yet) is three quanta within 15–200 ms.
    fn target(want: usize, pump_target: usize, channels: usize) -> usize {
        match pump_target * channels {
            0 => (3 * want).clamp(720 * channels, 9600 * channels),
            pump => want + pump,
        }
    }

    /// One quantum into `out` as F32LE: drop the oldest beyond `target` plus one quantum of
    /// slack, prime once the ring holds `target`, then drain. Silence while priming and on
    /// an underrun.
    fn fill(&mut self, ring: &mut VecDeque<f32>, out: &mut [u8], target: usize) -> Filled {
        let want = out.len() / 4;
        let excess = ring.len().saturating_sub(target.max(want) + want);
        ring.drain(..excess);
        if !self.primed && ring.len() >= target {
            self.primed = true;
        }
        for slot in out.chunks_exact_mut(4) {
            let s = match self.primed {
                true => ring.pop_front().unwrap_or(0.0),
                false => 0.0,
            };
            slot.copy_from_slice(&s.to_le_bytes());
        }
        let reprimed = self.primed && ring.is_empty();
        if reprimed {
            self.primed = false;
        }
        Filled {
            dropped: excess,
            reprimed,
        }
    }
}

fn mic_pw_thread(
    pcm_rx: Receiver<(std::time::Instant, Vec<f32>)>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    channels: u32,
    node_name: &str,
    flush: Arc<AtomicBool>,
    shared: Arc<MicRingShared>,
    ready: std::sync::mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::AudioInfoRaw;
    use spa::pod::Pod;

    // PipeWire objects are lifetime-chained (guards borrow mainloop/core), so
    // setup and the blocking run share one frame; the IIFE funnels every
    // setup `?` through the ready handshake.
    let result = (|| -> Result<()> {
        let (mainloop, core) = pw_setup::pw_connect("pw mic")?;

        let _quit_guard = quit_rx.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            move |_| mainloop.quit()
        });

        // Core error (daemon gone — our node is gone) ends this thread and
        // flips `alive` so the pump reopens. Without this, a restart leaves
        // the loop idling on a dead connection and the mic silent for life.
        let _core_listener = core
            .add_listener_local()
            .error({
                let mainloop = mainloop.clone();
                move |id, _seq, res, message| {
                    tracing::warn!(
                        id,
                        res,
                        message,
                        "pipewire core error — virtual mic reopening"
                    );
                    mainloop.quit();
                }
            })
            .register();

        // `Audio/Source` advertises a recordable microphone. Without it,
        // Direction::Output + Playback would route to the speakers.
        let stream = pw::stream::StreamBox::new(
            &core,
            node_name,
            properties! {
                *pw::keys::MEDIA_TYPE        => "Audio",
                *pw::keys::MEDIA_CLASS       => "Audio/Source",
                *pw::keys::NODE_NAME         => node_name,
                *pw::keys::NODE_DESCRIPTION  => "Punktfunk Remote Microphone",
                // ~5 ms quantum (one Opus frame) so recorders get low-latency chunks.
                *pw::keys::NODE_LATENCY      => "240/48000",
                // Lose default-source election: a session claims the default
                // (`stream_sink::SOURCE`), so the box's own mic stays default when
                // nobody streams. Hardware sits around 1000–1900.
                "priority.session"           => "50",
            },
        )
        .context("pw mic Stream")?;

        let ud = MicUserData {
            rx: pcm_rx,
            ring: VecDeque::new(),
            channels: channels as usize,
            gate: MicGate::default(),
            flush,
            shared,
            last_run: None,
        };

        let _listener = stream
            .add_local_listener_with_user_data(ud)
            .state_changed({
                let mainloop = mainloop.clone();
                move |_s, _ud, old, new| {
                    tracing::debug!(?old, ?new, "pipewire virtual-mic stream state");
                    // Unrecoverable for this instance — exit so the pump reopens.
                    if matches!(new, pw::stream::StreamState::Error(_)) {
                        mainloop.quit();
                    }
                }
            })
            .param_changed(|_s, _ud, id, param| {
                let Some(param) = param else { return };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = AudioInfoRaw::default();
                if info.parse(param).is_ok() {
                    tracing::info!(
                        format = ?info.format(),
                        rate = info.rate(),
                        channels = info.channels(),
                        "virtual-mic format negotiated"
                    );
                }
            })
            .process(|stream, ud| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        return;
                    };
                    // This cycle's quantum. The mapped buffer is sized for `quantum-limit`
                    // (8192 ≈ 170 ms); filling it primes and queues 170 ms a cycle. 0 → capacity.
                    let requested = usize::try_from(buffer.requested()).unwrap_or(0);
                    // Before pulling new frames: drop the ring on flush (uplink
                    // gap) or when this callback has not run for `MIC_STALE`
                    // (idle, no recorder). A recorder must not hear old audio.
                    let now = std::time::Instant::now();
                    let idled = ud
                        .last_run
                        .is_some_and(|t| now.duration_since(t) > MIC_STALE);
                    if ud.flush.swap(false, std::sync::atomic::Ordering::AcqRel) || idled {
                        ud.ring.clear();
                        ud.gate = MicGate::default();
                    }
                    ud.last_run = Some(now);
                    while let Ok((t, frame)) = ud.rx.try_recv() {
                        if now.duration_since(t) <= MIC_STALE {
                            ud.ring.extend(frame);
                        }
                    }
                    let stride = 4 * ud.channels; // F32LE interleaved
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        return;
                    }
                    let data = &mut datas[0];
                    let max_frames = data.data().map(|s| s.len() / stride).unwrap_or(0);
                    let want_frames = match requested {
                        0 => max_frames,
                        r => r.min(max_frames),
                    };
                    let want = want_frames * ud.channels;
                    static FIRST: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(true);
                    if FIRST.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        tracing::info!(
                            quantum_frames = want_frames,
                            capacity_frames = max_frames,
                            quantum_ms = want_frames as f32 / 48.0,
                            "virtual-mic consumer connected"
                        );
                    }

                    let pump_target = ud.shared.target.load(Ordering::Relaxed);
                    let target = MicGate::target(want, pump_target, ud.channels);
                    // A missing mapping has no capacity, so `want` is 0 there.
                    let out = data.data().map(|s| &mut s[..want * 4]).unwrap_or_default();
                    let filled = ud.gate.fill(&mut ud.ring, out, target);
                    if filled.dropped > 0 {
                        ud.shared
                            .overflow
                            .fetch_add((filled.dropped / ud.channels) as u64, Ordering::Relaxed);
                    }
                    if filled.reprimed {
                        ud.shared.reprimes.fetch_add(1, Ordering::Relaxed);
                    }
                    ud.shared
                        .depth
                        .store(ud.ring.len() / ud.channels, Ordering::Relaxed);
                    ud.shared
                        .prime
                        .store(target / ud.channels, Ordering::Relaxed);
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = stride as _;
                    *chunk.size_mut() = (stride * want_frames) as _;
                }));
                if outcome.is_err() {
                    tracing::error!("panic in pipewire virtual-mic callback");
                }
            })
            .register()
            .context("register virtual-mic stream listener")?;

        let values =
            pw_setup::f32_format_pod(SAMPLE_RATE, channels, spa_positions(channels as u8))?;
        let mut params = [Pod::from_bytes(&values).context("mic pod from bytes")?];

        // Run the producer on PipeWire's realtime data loop so the source is a
        // synchronous graph node that joins its consumer's driver group.
        // Without it the node is async and, on a busy multi-stream graph, never
        // acquires a driver — `process()` never fires, recorders hear silence.
        stream
            .connect(
                spa::utils::Direction::Output,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .context("pw mic stream connect")?;

        // Daemon + stream connect succeeded. A PipeWire that isn't running
        // never reaches here; its connect error is an open failure so the
        // pump backs off instead of churning on dead instances.
        let _ = ready.send(Ok(()));
        mainloop.run();
        tracing::debug!("pipewire virtual-mic loop exited (source dropped)");
        Ok(())
    })();
    if let Err(e) = &result {
        let _ = ready.send(Err(anyhow!("{e:#}")));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs one quantum of `want` samples and returns what was written.
    fn run(
        gate: &mut MicGate,
        ring: &mut VecDeque<f32>,
        want: usize,
        target: usize,
    ) -> (Vec<f32>, Filled) {
        let mut out = vec![0xAAu8; want * 4];
        let filled = gate.fill(ring, &mut out, target);
        let samples = out
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        (samples, filled)
    }

    #[test]
    fn silence_until_primed_then_drain() {
        let mut gate = MicGate::default();
        let mut ring: VecDeque<f32> = (1..=3).map(|v| v as f32).collect();
        let (out, filled) = run(&mut gate, &mut ring, 2, 4);
        assert_eq!(out, [0.0, 0.0], "below the prime depth: silence, ring kept");
        assert_eq!(ring.len(), 3);
        assert_eq!(
            filled,
            Filled {
                dropped: 0,
                reprimed: false
            }
        );
        ring.push_back(4.0);
        assert_eq!(run(&mut gate, &mut ring, 2, 4).0, [1.0, 2.0]);
        // Primed: holds below the prime depth until the ring is empty.
        assert_eq!(run(&mut gate, &mut ring, 2, 4).0, [3.0, 4.0]);
    }

    #[test]
    fn an_underrun_pads_silence_and_rearms() {
        let mut gate = MicGate::default();
        let mut ring: VecDeque<f32> = [1.0, 2.0, 3.0].into();
        let (out, filled) = run(&mut gate, &mut ring, 4, 3);
        assert_eq!(out, [1.0, 2.0, 3.0, 0.0]);
        assert_eq!(
            filled,
            Filled {
                dropped: 0,
                reprimed: true
            }
        );
        ring.push_back(5.0);
        assert_eq!(run(&mut gate, &mut ring, 2, 3).0, [0.0, 0.0], "re-priming");
    }

    #[test]
    fn overflow_drops_the_oldest_beyond_one_quantum_of_slack() {
        let mut gate = MicGate::default();
        let mut ring: VecDeque<f32> = (0..10).map(|v| v as f32).collect();
        let (out, filled) = run(&mut gate, &mut ring, 2, 4);
        // Bound is target + want = 6: the four oldest go.
        assert_eq!(filled.dropped, 4);
        assert_eq!(out, [4.0, 5.0]);
        assert_eq!(ring.len(), 4);
        // An empty quantum (no mapped buffer) still bounds the ring.
        let mut big: VecDeque<f32> = (0..10).map(|v| v as f32).collect();
        assert_eq!(gate.fill(&mut big, &mut [], 4).dropped, 6);
    }

    #[test]
    fn prime_target_follows_the_pump_or_three_clamped_quanta() {
        // Stereo, 240-frame quanta: 480 samples each.
        assert_eq!(MicGate::target(480, 0, 2), 1440, "three quanta");
        assert_eq!(MicGate::target(64, 0, 2), 1440, "15 ms floor");
        assert_eq!(MicGate::target(8192 * 2, 0, 2), 19200, "200 ms ceiling");
        assert_eq!(
            MicGate::target(480, 960, 2),
            480 + 1920,
            "quantum + pump target"
        );
    }
}
