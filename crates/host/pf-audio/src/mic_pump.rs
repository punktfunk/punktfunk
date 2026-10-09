//! Host-lifetime virtual-microphone pump.
//!
//! One thread owns the [`VirtualMic`] backend and an Opus decoder. Sessions
//! `try_send` client `0xCB` frames onto a clonable sender; the thread de-jitters,
//! decodes, and feeds PCM so host apps hear the client's mic. Opens at host
//! start (games bind capture once and never re-follow), reopens a dead backend
//! with backoff, and discards buffered audio after an uplink gap. Decode errors
//! drop that frame only. The thread exits when every sender is dropped.
//!
//! Pin via [`MicPump::start`] / [`MicPump::start_named`]. Evidence: `pump_tests`.
//! [`VirtualMic`], [`open_virtual_mic`](super::open_virtual_mic), and
//! [`SAMPLE_RATE`](super::SAMPLE_RATE) stay in `super`.

use super::mic_jitter::{Deliver, MicDejitter};
use super::{VirtualMic, SAMPLE_RATE};
use anyhow::Result;

/// Stereo: the Opus decoder and the host→client layout are both 2ch.
pub const MIC_CHANNELS: u32 = 2;

/// One `0xCB` uplink frame (`punktfunk_core::quic::decode_mic_datagram`).
/// `seq`/`pts_ns` ride with the Opus payload so de-jitter can reorder, conceal, and track cadence.
pub struct MicFrame {
    /// Sending session ([`mic_source_id`]). One pump has one decoder: one source at a time.
    pub source: u64,
    pub seq: u32,
    pub pts_ns: u64,
    pub opus: Vec<u8>,
}

/// A fresh [`MicFrame::source`] for a session's uplink.
pub fn mic_source_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Drop-newest bound on the host-lifetime queue: 12 × 5–20 ms ≈ 60–240 ms of slack.
/// Shared across sessions; [`DRAIN_ABOVE`] heals anything deeper.
const MIC_QUEUE_CAP: usize = 12;
/// Wake deeper than this: keep the newest [`DRAIN_KEEP`]. Replaying late frames turns a stall into standing delay.
const DRAIN_ABOVE: usize = 6;
const DRAIN_KEEP: usize = 4;

const TELEMETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Shed only when depth stays > target by this many ms for [`TRIM_AFTER`], then at most
/// one silent frame per [`TRIM_SPACING`] (20 ms / 300 ms ≈ 7 ms per 100 ms). A burst otherwise
/// only comes back down on a full drain.
const TRIM_MARGIN_MS: usize = 15;
const TRIM_AFTER: std::time::Duration = std::time::Duration::from_secs(2);
const TRIM_SPACING: std::time::Duration = std::time::Duration::from_millis(300);
/// ≈ −48 dBFS; only near-silent frames may be shed.
const TRIM_SILENCE_PEAK: f32 = 0.004;

/// Open/reopen/flush delays; tests pass millisecond values so the real loop still runs.
#[derive(Clone, Copy)]
struct PumpTuning {
    /// First retry after a failed open; doubles up to `backoff_cap` so a missing endpoint is not hammered.
    backoff_start: std::time::Duration,
    backoff_cap: std::time::Duration,
    /// Idle liveness probe: a dead backend reopens before the next session starts.
    heartbeat: std::time::Duration,
    /// Uplink gap longer than this: discard buffered audio so a recorder never hears a mute-era burst.
    stale_gap: std::time::Duration,
    /// Died before this: treat as a failed open (flapping daemon must not churn at heartbeat rate). Lived longer: reset backoff.
    stable_after: std::time::Duration,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
const PUMP_TUNING: PumpTuning = PumpTuning {
    backoff_start: std::time::Duration::from_secs(2),
    backoff_cap: std::time::Duration::from_secs(60),
    heartbeat: std::time::Duration::from_secs(1),
    stale_gap: std::time::Duration::from_millis(600),
    stable_after: std::time::Duration::from_secs(5),
};

/// One thread owns [`VirtualMic`] plus an Opus decoder; sessions clone a `Send` sender for `0xCB`.
///
/// Opens at host start (games bind capture once). Reopens on push-fail or idle heartbeat
/// without invalidating senders. Discards buffered audio after an uplink gap. De-jitters via
/// [`MicDejitter`](super::mic_jitter) (`PUNKTFUNK_MIC_LEGACY_BUFFER=1` pins the old fixed prime).
/// Per-frame decode errors drop that frame. Exits when every sender is dropped.
pub struct MicPump {
    tx: std::sync::mpsc::SyncSender<MicFrame>,
}

impl MicPump {
    /// Host-lifetime pump. Linux/Windows open a backend; other platforms drain and drop (sessions still count datagrams).
    pub fn start() -> MicPump {
        Self::start_named(None)
    }

    /// [`start`](Self::start) with a `node.name`. `Some` is a session-lifetime source for an
    /// isolated gamescope session (`design/gamescope-multiuser.md`): `punktfunk-mic-{id}`, fed
    /// only by that session, torn down when the owner and its sender clone drop. `None` is the
    /// shared `punktfunk-mic`.
    pub fn start_named(source_name: Option<String>) -> MicPump {
        let (tx, rx) = std::sync::mpsc::sync_channel::<MicFrame>(MIC_QUEUE_CAP);
        let spawned = std::thread::Builder::new()
            .name("punktfunk-mic-pump".into())
            .spawn(move || {
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                pump_thread(
                    rx,
                    move || super::open_virtual_mic_named(MIC_CHANNELS, source_name.as_deref()),
                    PUMP_TUNING,
                );
                #[cfg(not(any(target_os = "linux", target_os = "windows")))]
                {
                    let _ = source_name;
                    tracing::warn!("mic passthrough unsupported on this platform — frames dropped");
                    for _ in rx {}
                }
            });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "mic pump thread spawn failed — mic passthrough disabled");
        }
        MicPump { tx }
    }

    /// Session clone: `try_send` so a datagram loop never blocks. Dropping a clone does not stop
    /// the pump — it holds the original sender for the host life.
    pub fn sender(&self) -> std::sync::mpsc::SyncSender<MicFrame> {
        self.tx.clone()
    }
}

/// Sleep `dur` while dropping queued frames so a closed backend cannot wedge senders or keep a stale backlog. `false` = every sender gone.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
fn drain_sleep(rx: &std::sync::mpsc::Receiver<MicFrame>, dur: std::time::Duration) -> bool {
    use std::sync::mpsc::RecvTimeoutError;
    let deadline = std::time::Instant::now() + dur;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        match rx.recv_timeout(left.min(std::time::Duration::from_millis(250))) {
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Sheds one near-silent frame when the backend's depth has stayed past its target by
/// [`TRIM_MARGIN_MS`] for [`TRIM_AFTER`], at most one per [`TRIM_SPACING`]: never speech,
/// never a hard clear. Time is passed in.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
struct Trim {
    over_since: Option<std::time::Instant>,
    last: std::time::Instant,
}

#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
impl Trim {
    /// `depth` is `(buffered, target)` per-channel samples; `silent` scans the frame and runs
    /// only once the depth and the clocks allow a shed.
    fn shed(
        &mut self,
        now: std::time::Instant,
        depth: Option<(usize, usize)>,
        silent: impl FnOnce() -> bool,
    ) -> bool {
        match depth {
            Some((buffered, target))
                if buffered > target + TRIM_MARGIN_MS * SAMPLE_RATE as usize / 1000 =>
            {
                let since = *self.over_since.get_or_insert(now);
                let shed = now.duration_since(since) >= TRIM_AFTER
                    && now.duration_since(self.last) >= TRIM_SPACING
                    && silent();
                if shed {
                    self.last = now;
                }
                shed
            }
            _ => {
                self.over_since = None;
                false
            }
        }
    }
}

/// Why one backend's [`PumpLife::run`] ended.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
enum LifeEnd {
    /// The backend died: reopen it.
    Died,
    /// Every sender is gone: the host is shutting down.
    Stopped,
}

/// One open backend: its decoder, de-jitter, PCM scratch, trim state and the telemetry
/// window. A reopen starts a fresh one.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
struct PumpLife {
    mic: Box<dyn VirtualMic>,
    decoder: opus::Decoder,
    tuning: PumpTuning,
    /// `PUNKTFUNK_MIC_LEGACY_BUFFER=1`: the backend keeps its fixed prime, and nothing trims.
    legacy: bool,
    jitter: MicDejitter,
    pcm: Vec<f32>,
    /// libopus sizes PLC from the output slice; 960 = 20 ms until a real frame sets it.
    plc_samples: usize,
    decode_fails: u64,
    applied_target_ms: u32,
    last_push: std::time::Instant,
    trim: Trim,
    /// The session whose uplink the decoder follows, and its last frame. Another session's
    /// frames wait until it has been quiet for `stale_gap`: two Opus streams through one
    /// decoder and one sequence chain garble both.
    floor: Option<(u64, std::time::Instant)>,
    batch: Vec<MicFrame>,
    deliveries: Vec<Deliver>,
    // The telemetry window, reset on read.
    frames_seen: u64,
    drain_drops: u64,
    trimmed: u64,
    floor_drops: u64,
    last_log: std::time::Instant,
}

#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
impl PumpLife {
    fn new(mic: Box<dyn VirtualMic>, decoder: opus::Decoder, tuning: PumpTuning) -> PumpLife {
        let now = std::time::Instant::now();
        PumpLife {
            mic,
            decoder,
            tuning,
            legacy: super::mic_legacy_buffer(),
            jitter: MicDejitter::new(),
            pcm: vec![0f32; 5760 * MIC_CHANNELS as usize], // 120 ms at 48 kHz
            plc_samples: 960,
            decode_fails: 0,
            applied_target_ms: 0,
            last_push: now,
            trim: Trim {
                over_since: None,
                last: now,
            },
            floor: None,
            batch: Vec::new(),
            deliveries: Vec::new(),
            frames_seen: 0,
            drain_drops: 0,
            trimmed: 0,
            floor_drops: 0,
            last_log: now,
        }
    }

    fn run(&mut self, rx: &std::sync::mpsc::Receiver<MicFrame>) -> LifeEnd {
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::Instant;
        loop {
            // Soonest of heartbeat and a parked reorder hold aging out.
            let timeout = self
                .jitter
                .hold_deadline()
                .map(|d| d.saturating_duration_since(Instant::now()))
                .unwrap_or(self.tuning.heartbeat)
                .min(self.tuning.heartbeat);
            self.deliveries.clear();
            match rx.recv_timeout(timeout) {
                Ok(first) => self.ingest(first, rx),
                Err(RecvTimeoutError::Timeout) => {
                    self.jitter
                        .flush_expired_hold(Instant::now(), &mut self.deliveries);
                    if !self.mic.alive() {
                        tracing::warn!("virtual mic backend died while idle — reopening");
                        return LifeEnd::Died;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    tracing::debug!("mic pump stopped (host shutting down)");
                    return LifeEnd::Stopped;
                }
            }
            if !self.deliver() {
                tracing::warn!("virtual mic backend died — reopening");
                return LifeEnd::Died;
            }
            // Legacy mode leaves the backend on its fixed prime (`VirtualMic::set_target_depth`).
            if !self.legacy {
                let t = self.jitter.target_ms(Instant::now());
                if t != self.applied_target_ms {
                    self.applied_target_ms = t;
                    self.mic
                        .set_target_depth(t as usize * SAMPLE_RATE as usize / 1000);
                }
            }
            self.log_window();
        }
    }

    /// Take `first` and whatever queued behind it into the de-jitter.
    fn ingest(&mut self, first: MicFrame, rx: &std::sync::mpsc::Receiver<MicFrame>) {
        // After a stall the backlog is standing latency; jump to the newest DRAIN_KEEP.
        self.batch.clear();
        self.batch.push(first);
        while self.batch.len() <= MIC_QUEUE_CAP {
            match rx.try_recv() {
                Ok(f) => self.batch.push(f),
                Err(_) => break,
            }
        }
        if self.batch.len() > DRAIN_ABOVE {
            let drop_n = self.batch.len() - DRAIN_KEEP;
            self.drain_drops += drop_n as u64;
            self.batch.drain(..drop_n);
            // Dropped on purpose: concealing those seqs would put the latency back.
            self.jitter.reset_stream();
        }
        self.frames_seen += self.batch.len() as u64;
        if self.last_push.elapsed() > self.tuning.stale_gap {
            self.mic.discard();
            self.jitter.reset_stream();
        }
        let now = std::time::Instant::now();
        for frame in self.batch.drain(..) {
            match self.floor {
                Some((s, at)) if s != frame.source => {
                    if now.duration_since(at) <= self.tuning.stale_gap {
                        self.floor_drops += 1;
                        continue;
                    }
                    self.jitter.reset_stream();
                    let _ = self.decoder.reset_state();
                }
                _ => {}
            }
            self.floor = Some((frame.source, now));
            self.jitter.ingest(now, frame, &mut self.deliveries);
        }
        // Traffic that is only late duplicates never hits the timeout arm; still flush an expired hold.
        self.jitter.flush_expired_hold(now, &mut self.deliveries);
    }

    /// Decode and push what the de-jitter released. `false` = the backend died.
    fn deliver(&mut self) -> bool {
        let mut deliveries = std::mem::take(&mut self.deliveries);
        let alive = deliveries.drain(..).all(|d| self.deliver_one(d));
        self.deliveries = deliveries;
        alive
    }

    fn deliver_one(&mut self, d: Deliver) -> bool {
        let samples_per_ch = match d {
            Deliver::Frame(frame) => {
                if frame.opus.is_empty() {
                    return true; // DTX — the source underruns to silence on its own
                }
                match self.decoder.decode_float(&frame.opus, &mut self.pcm, false) {
                    Ok(n) => {
                        self.plc_samples = n.max(120); // ≥ 2.5 ms; shorter slices mis-size libopus PLC
                        self.decode_fails = 0;
                        n
                    }
                    Err(e) => {
                        self.decode_fails += 1;
                        if self.decode_fails.is_power_of_two() {
                            tracing::warn!(error = %e, fails = self.decode_fails,
                                "mic opus decode failed — dropping frame");
                        }
                        return true;
                    }
                }
            }
            Deliver::Conceal => {
                // Empty-input decode = libopus PLC for this slice, so one gap does not starve the ring into a re-prime.
                let want = (self.plc_samples * MIC_CHANNELS as usize).min(self.pcm.len());
                match self.decoder.decode_float(&[], &mut self.pcm[..want], false) {
                    Ok(n) => n,
                    Err(_) => return true, // nothing decoded yet — nothing to extend
                }
            }
        };
        let total = (samples_per_ch * MIC_CHANNELS as usize).min(self.pcm.len());
        let pcm = &self.pcm[..total];
        let now = std::time::Instant::now();
        if !self.legacy
            && self.trim.shed(now, self.mic.depth(), || {
                pcm.iter().all(|s| s.abs() < TRIM_SILENCE_PEAK)
            })
        {
            self.trimmed += 1;
            self.last_push = now; // trim is not a stale gap
            return true;
        }
        if !self.mic.push(pcm) {
            return false;
        }
        self.last_push = std::time::Instant::now();
        true
    }

    /// One `mic uplink health` line per [`TELEMETRY_EVERY`], skipped when the window saw no
    /// frames. Every counter resets on read.
    fn log_window(&mut self) {
        if self.last_log.elapsed() < TELEMETRY_EVERY {
            return;
        }
        if self.frames_seen > 0 {
            let js = self.jitter.take_stats();
            let bs = self.mic.take_stats();
            let (depth_ms, target_ms) = self
                .mic
                .depth()
                .map(|(d, t)| {
                    (
                        d * 1000 / SAMPLE_RATE as usize,
                        t * 1000 / SAMPLE_RATE as usize,
                    )
                })
                .unwrap_or((0, 0));
            tracing::info!(
                depth_ms,
                target_ms,
                cadence_ms = (js.cadence_ms * 10.0).round() / 10.0,
                frame_ms = (js.frame_ms * 10.0).round() / 10.0,
                frames = self.frames_seen,
                gaps = js.seq_gaps,
                concealed = js.concealed,
                reorders = js.reorders,
                late = js.late_drops,
                drained = self.drain_drops,
                // Frames from a second session while another held the mic.
                other_session = self.floor_drops,
                trimmed = self.trimmed,
                reprimes = bs.reprimes,
                overflow_ms = bs.overflow_dropped * 1000 / SAMPLE_RATE as u64,
                "mic uplink health"
            );
        }
        self.last_log = std::time::Instant::now();
        self.frames_seen = 0;
        self.drain_drops = 0;
        self.floor_drops = 0;
        self.trimmed = 0;
    }
}

/// Pump loop: open with backoff, run one [`PumpLife`] per backend, and triage its death.
/// `opener` is injected so tests run this loop against a mock; production uses
/// [`open_virtual_mic`](super::open_virtual_mic).
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
fn pump_thread<O>(rx: std::sync::mpsc::Receiver<MicFrame>, opener: O, tuning: PumpTuning)
where
    O: Fn() -> Result<Box<dyn VirtualMic>>,
{
    let mut backoff = tuning.backoff_start;
    let mut open_fails: u64 = 0;
    loop {
        let (mic, decoder) = loop {
            let opened = opener().and_then(|m| {
                let d = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)
                    .map_err(|e| anyhow::anyhow!("opus decoder: {e}"))?;
                Ok((m, d))
            });
            match opened {
                Ok(pair) => break pair,
                Err(e) => {
                    // Power-of-two log: a missing endpoint would otherwise warn every backoff forever.
                    open_fails += 1;
                    if open_fails.is_power_of_two() {
                        tracing::warn!(error = %format!("{e:#}"), attempts = open_fails,
                            "virtual mic unavailable — retrying with backoff");
                    }
                    if !drain_sleep(&rx, backoff) {
                        return;
                    }
                    backoff = (backoff * 2).min(tuning.backoff_cap);
                }
            }
        };
        tracing::info!("virtual mic ready (host-lifetime)");
        // Queued frames predate this backend. Backoff resets only after a stable life (death triage below).
        while rx.try_recv().is_ok() {}
        let opened_at = std::time::Instant::now();
        if let LifeEnd::Stopped = PumpLife::new(mic, decoder, tuning).run(&rx) {
            return;
        }
        // Lived ≥ stable_after: one-off death, reset backoff. Died earlier: failed open — back off or the pump churns at heartbeat rate.
        if opened_at.elapsed() >= tuning.stable_after {
            backoff = tuning.backoff_start;
            open_fails = 0;
        } else {
            open_fails += 1;
            if !drain_sleep(&rx, backoff) {
                return;
            }
            backoff = (backoff * 2).min(tuning.backoff_cap);
        }
    }
}

#[cfg(test)]
mod pump_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Only a near-silent frame sheds, only after the depth has stayed over target for
    /// `TRIM_AFTER`, and no more than one per `TRIM_SPACING`. Dipping under resets the clock.
    #[test]
    fn trim_sheds_only_sustained_near_silence() {
        let ms = std::time::Duration::from_millis;
        let t0 = std::time::Instant::now();
        let over = Some((48_000, 2_400)); // 1 s buffered over a 50 ms target
        let mut trim = Trim {
            over_since: None,
            last: t0,
        };
        let quiet = || true;
        assert!(!trim.shed(t0, over, quiet), "not over for long enough");
        assert!(
            !trim.shed(t0 + ms(2_000), over, || false),
            "speech is never shed"
        );
        assert!(trim.shed(t0 + ms(2_000), over, quiet));
        assert!(!trim.shed(t0 + ms(2_100), over, quiet), "one per spacing");
        assert!(trim.shed(t0 + ms(2_300), over, quiet));
        assert!(
            !trim.shed(t0 + ms(2_600), Some((2_400, 2_400)), quiet),
            "at target"
        );
        assert!(
            !trim.shed(t0 + ms(3_000), over, quiet),
            "the clock restarted"
        );
        assert!(!trim.shed(t0 + ms(3_000), None, quiet), "no ring, no trim");
    }

    struct MockMic {
        alive: Arc<AtomicBool>,
        polled: Arc<AtomicBool>,
        pushed: Arc<AtomicUsize>,
        discards: Arc<AtomicUsize>,
        /// While set, `push` stalls after counting the call: a backlog builds behind it.
        gate: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }
    impl VirtualMic for MockMic {
        fn push(&self, pcm: &[f32]) -> bool {
            if !self.alive.load(Ordering::Acquire) {
                return false;
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            while self.gate.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.pushed.fetch_add(pcm.len(), Ordering::Relaxed);
            true
        }
        fn alive(&self) -> bool {
            self.polled.store(true, Ordering::Release);
            self.alive.load(Ordering::Acquire)
        }
        fn discard(&self) {
            self.discards.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct Harness {
        tx: std::sync::mpsc::SyncSender<MicFrame>,
        opens: Arc<AtomicUsize>,
        alive: Arc<Mutex<Option<Arc<AtomicBool>>>>, // latest instance's kill switch
        // Set by the pump's first idle `alive()` poll, which runs after its open-time drain.
        // `alive` is set inside the opener, before that drain, so a frame sent on it can vanish.
        polled: Arc<AtomicBool>,
        pushed: Arc<AtomicUsize>,
        discards: Arc<AtomicUsize>,
        gate: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
        join: std::thread::JoinHandle<()>,
    }

    /// The harness's `stale_gap`: short, so a test that waits one out stays fast.
    const STALE_GAP: Duration = Duration::from_millis(80);
    /// For a test that needs the gap not to pass: past any stall a loaded runner produces.
    const LONG_GAP: Duration = Duration::from_secs(1);

    /// Real loop vs mocks. `fail_first` = open failures before success. `dead_on_arrival` = every
    /// instance pre-killed. `stable_after = ZERO` treats every death as stable so tests stay fast.
    fn start_tuned(
        fail_first: usize,
        dead_on_arrival: bool,
        stable_after: Duration,
        stale_gap: Duration,
    ) -> Harness {
        let (tx, rx) = std::sync::mpsc::sync_channel::<MicFrame>(MIC_QUEUE_CAP);
        let opens = Arc::new(AtomicUsize::new(0));
        let alive = Arc::new(Mutex::new(None::<Arc<AtomicBool>>));
        let polled = Arc::new(AtomicBool::new(false));
        let pushed = Arc::new(AtomicUsize::new(0));
        let discards = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let (opens2, alive2, polled2, pushed2, discards2) = (
            opens.clone(),
            alive.clone(),
            polled.clone(),
            pushed.clone(),
            discards.clone(),
        );
        let (gate2, calls2) = (gate.clone(), calls.clone());
        let tuning = PumpTuning {
            backoff_start: Duration::from_millis(10),
            backoff_cap: Duration::from_millis(40),
            heartbeat: Duration::from_millis(20),
            stale_gap,
            stable_after,
        };
        let join = std::thread::spawn(move || {
            pump_thread(
                rx,
                move || {
                    let n = opens2.fetch_add(1, Ordering::SeqCst);
                    if n < fail_first {
                        anyhow::bail!("backend not up yet (simulated)");
                    }
                    let a = Arc::new(AtomicBool::new(!dead_on_arrival));
                    *alive2.lock().unwrap() = Some(a.clone());
                    Ok(Box::new(MockMic {
                        alive: a,
                        polled: polled2.clone(),
                        pushed: pushed2.clone(),
                        discards: discards2.clone(),
                        gate: gate2.clone(),
                        calls: calls2.clone(),
                    }) as Box<dyn VirtualMic>)
                },
                tuning,
            )
        });
        Harness {
            tx,
            opens,
            alive,
            polled,
            pushed,
            discards,
            gate,
            calls,
            join,
        }
    }

    fn start(fail_first: usize) -> Harness {
        start_tuned(fail_first, false, Duration::ZERO, STALE_GAP)
    }

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..600 {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for: {what}");
    }

    /// Keep sending until PCM hits the backend.
    ///
    /// One frame is not enough after reopen: the open path drains the queue (that audio predates
    /// the device), and `opens` ticks at the *start* of open, so a send on the counter bump lands
    /// in that drain. Seq must advance or de-jitter drops repeats as duplicates.
    fn wait_until_pushed(what: &str, h: &Harness, from_seq: u32) {
        let mut seq = from_seq;
        for _ in 0..600 {
            let _ = h.tx.try_send(mic_frame(seq));
            seq = seq.wrapping_add(1);
            if h.pushed.load(Ordering::SeqCst) > 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for: {what}");
    }

    fn opus_frame() -> Vec<u8> {
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip)
            .expect("opus encoder");
        let pcm = [0.1f32; 960 * 2]; // 20 ms stereo
        let mut out = vec![0u8; 4000];
        let n = enc.encode_float(&pcm, &mut out).expect("encode");
        out.truncate(n);
        out
    }

    /// `pts_ns = seq * 20 ms`; payload from [`opus_frame`].
    fn mic_frame(seq: u32) -> MicFrame {
        MicFrame {
            source: 1,
            seq,
            pts_ns: seq as u64 * 20_000_000,
            opus: opus_frame(),
        }
    }

    /// One decoder follows one session. A second session's frames wait until the first has
    /// gone quiet for `stale_gap`, then take over; interleaving the two garbled both.
    #[test]
    fn a_second_session_waits_until_the_first_goes_quiet() {
        // A stall past `stale_gap` between the two sessions' frames hands the floor over.
        let h = start_tuned(0, false, Duration::ZERO, LONG_GAP);
        wait_until("pump polled", || h.polled.load(Ordering::Acquire));
        let frame = 960 * MIC_CHANNELS as usize;
        let other = |seq| MicFrame {
            source: 2,
            ..mic_frame(seq)
        };
        for seq in 0..5u32 {
            h.tx.send(mic_frame(seq)).unwrap();
            h.tx.send(other(seq)).unwrap();
            // Paced on the pump: a backlog past `DRAIN_ABOVE` is dropped, not played.
            wait_until("floor frame pushed", || {
                h.pushed.load(Ordering::SeqCst) >= (seq as usize + 1) * frame
            });
        }
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            h.pushed.load(Ordering::SeqCst),
            5 * frame,
            "only the floor's frames"
        );
        std::thread::sleep(LONG_GAP + Duration::from_millis(40));
        for seq in 5..8 {
            h.tx.send(other(seq)).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        wait_until("second session took over", || {
            h.pushed.load(Ordering::SeqCst) >= 8 * frame
        });
        drop(h.tx);
        h.join.join().unwrap();
    }

    /// A backlog the pump drains is skipped, not concealed: only the kept frames reach the mic.
    #[test]
    fn drained_backlog_is_not_concealed() {
        let h = start(0);
        wait_until("pump polled", || h.polled.load(Ordering::Acquire));
        // Encode first: the stall must stay well inside `stale_gap`.
        let backlog: Vec<MicFrame> = (1..=MIC_QUEUE_CAP as u32).map(mic_frame).collect();
        h.gate.store(true, Ordering::Release);
        h.tx.send(mic_frame(0)).unwrap();
        wait_until("pump stalled in push", || {
            h.calls.load(Ordering::SeqCst) >= 1
        });
        for f in backlog {
            h.tx.try_send(f)
                .expect("queue has room for the whole backlog");
        }
        h.gate.store(false, Ordering::Release);
        let frame = 960 * MIC_CHANNELS as usize; // one 20 ms stereo frame
        let want = (1 + DRAIN_KEEP) * frame;
        wait_until("kept frames pushed", || {
            h.pushed.load(Ordering::SeqCst) >= want
        });
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            h.pushed.load(Ordering::SeqCst) / frame,
            1 + DRAIN_KEEP,
            "frame 0 plus the newest DRAIN_KEEP, with no concealment for the drained seqs"
        );
        drop(h.tx);
        h.join.join().unwrap();
    }

    #[test]
    fn opens_eagerly_with_backoff() {
        let h = start(3);
        wait_until("eager open after 3 failures", || {
            h.opens.load(Ordering::SeqCst) >= 4 && h.alive.lock().unwrap().is_some()
        });
        drop(h.tx);
        h.join.join().unwrap();
    }

    #[test]
    fn decodes_and_pushes() {
        let h = start(0);
        wait_until("pump polled", || h.polled.load(Ordering::Acquire));
        h.tx.send(mic_frame(0)).unwrap();
        wait_until("pcm pushed", || h.pushed.load(Ordering::SeqCst) > 0);
        drop(h.tx);
        h.join.join().unwrap();
    }

    #[test]
    fn reopens_after_idle_death() {
        let h = start(0);
        wait_until("first open", || h.opens.load(Ordering::SeqCst) >= 1);
        wait_until("instance", || h.alive.lock().unwrap().is_some());
        h.alive
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .store(false, Ordering::Release);
        wait_until("reopen after idle death", || {
            h.opens.load(Ordering::SeqCst) >= 2
        });
        drop(h.tx);
        h.join.join().unwrap();
    }

    #[test]
    fn reopens_after_push_death() {
        let h = start(0);
        wait_until("instance", || h.alive.lock().unwrap().is_some());
        h.alive
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .store(false, Ordering::Release);
        h.tx.send(mic_frame(0)).unwrap();
        wait_until("reopen", || h.opens.load(Ordering::SeqCst) >= 2);
        wait_until_pushed("pcm after reopen", &h, 1);
        drop(h.tx);
        h.join.join().unwrap();
    }

    #[test]
    fn rapid_death_backs_off() {
        // Dead on arrival; high stable_after so each death is a failed open.
        // Unguarded: ~25 opens / 500 ms at 20 ms heartbeat. Backoff 10→20→40: ≈ 7.
        let h = start_tuned(0, true, Duration::from_secs(10), STALE_GAP);
        std::thread::sleep(Duration::from_millis(500));
        let opens = h.opens.load(Ordering::SeqCst);
        assert!(opens >= 2, "must keep retrying (got {opens})");
        assert!(
            opens <= 15,
            "must back off, not churn per heartbeat (got {opens})"
        );
        drop(h.tx);
        h.join.join().unwrap();
    }

    /// seq 0 then 2 must push ~3 frames (decode, PLC, decode) once the reorder window expires.
    #[test]
    fn seq_gap_is_concealed() {
        // A stall past `stale_gap` between the two frames is a pause, which nothing conceals.
        let h = start_tuned(0, false, Duration::ZERO, LONG_GAP);
        wait_until("pump polled", || h.polled.load(Ordering::Acquire));
        h.tx.send(mic_frame(0)).unwrap();
        h.tx.send(mic_frame(2)).unwrap();
        // 0 plays now; 2 is held ≤ 30 ms for 1, then conceal + play 2.
        wait_until("conceal + late frame pushed", || {
            h.pushed.load(Ordering::SeqCst) >= 3 * 960 * 2
        });
        drop(h.tx);
        h.join.join().unwrap();
    }

    #[test]
    fn discards_after_gap() {
        let h = start(0);
        wait_until("pump polled", || h.polled.load(Ordering::Acquire));
        h.tx.send(mic_frame(0)).unwrap();
        wait_until("first push", || h.pushed.load(Ordering::SeqCst) > 0);
        std::thread::sleep(Duration::from_millis(150)); // > stale_gap
        h.tx.send(mic_frame(1)).unwrap();
        wait_until("discard on gap", || h.discards.load(Ordering::SeqCst) >= 1);
        drop(h.tx);
        h.join.join().unwrap();
    }
}
