//! Pure capture-policy decisions extracted from [`wasapi_cap`](super::wasapi_cap) so they compile
//! and their tests run on every platform, same as [`wiring_plan`](super::wiring_plan).
//!
//! * [`FightDamper`] — how hard to fight another program for the default playback device.
//! * [`CaptureStats`] / [`SendStats`] — capture and egress vitals for one reporting window.
//! * [`InfillPolicy`] — whether the wire covers a capture hole with silence.
//!
//! Time is passed in, never read here. Pin behaviour with the unit tests in this file.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Live `CLIENT_CAP_KEEP_HOST_AUDIO` asks. A count, not a flag: sessions overlap, and any live
/// asker wins host-wide until it ends. Windows reads it in `audio_control::keep_default_devices`;
/// Linux in the capturer's topology pick (Monitor — tap the default sink instead of claiming it).
static KEEP_HOST_AUDIO_SESSIONS: AtomicUsize = AtomicUsize::new(0);

/// One session's `CLIENT_CAP_KEEP_HOST_AUDIO` ask. Drop decrements on every exit, including panic.
pub struct KeepHostAudioGuard(());

pub fn keep_host_audio_guard() -> KeepHostAudioGuard {
    KEEP_HOST_AUDIO_SESSIONS.fetch_add(1, Ordering::Relaxed);
    KeepHostAudioGuard(())
}

impl Drop for KeepHostAudioGuard {
    fn drop(&mut self) {
        KEEP_HOST_AUDIO_SESSIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn session_keeps_default() -> bool {
    KEEP_HOST_AUDIO_SESSIONS.load(Ordering::Relaxed) > 0
}

/// Transient default-device churn settles; looping capture teardowns do not.
pub const FIGHT_LIMIT: u32 = 4;
pub const FIGHT_WINDOW: Duration = Duration::from_secs(20);
/// Do not fight again until this elapses; a program that keeps taking the default will win it.
pub const FIGHT_BACKOFF: Duration = Duration::from_secs(60);

/// Caps default-playback re-asserts: fight a few times, then concede for [`FIGHT_BACKOFF`].
///
/// Time is passed in so the policy stays pure and the tests run off Windows.
pub struct FightDamper {
    count: u32,
    window_started: Instant,
    paused_until: Option<Instant>,
    /// One warning per fight burst; one per concession.
    warned_fighting: bool,
    warned_giving_up: bool,
    now: Instant,
}

impl FightDamper {
    pub fn new(now: Instant) -> FightDamper {
        FightDamper {
            count: 0,
            window_started: now,
            paused_until: None,
            warned_fighting: false,
            warned_giving_up: false,
            now,
        }
    }

    pub fn observed_at(&mut self, now: Instant) {
        self.now = now;
        if now.duration_since(self.window_started) >= FIGHT_WINDOW {
            self.window_started = now;
            self.count = 0;
            self.warned_fighting = false;
        }
        if self.paused_until.is_some_and(|t| now >= t) {
            self.paused_until = None;
            self.warned_giving_up = false;
            self.count = 0;
            self.window_started = now;
        }
    }

    pub fn should_reassert(&mut self) -> bool {
        if self.paused_until.is_some() {
            return false;
        }
        if self.count >= FIGHT_LIMIT {
            self.paused_until = Some(self.now + FIGHT_BACKOFF);
            return false;
        }
        self.count += 1;
        true
    }

    /// First re-assert of a burst only; the rest are noise.
    pub fn warn_now(&mut self) -> bool {
        !std::mem::replace(&mut self.warned_fighting, true)
    }

    pub fn warn_giving_up(&mut self) -> bool {
        self.paused_until.is_some() && !std::mem::replace(&mut self.warned_giving_up, true)
    }

    pub fn is_paused(&self) -> bool {
        self.paused_until.is_some()
    }
}

pub const STATS_EVERY: Duration = Duration::from_secs(30);

/// Floor on a gap, even when `2 × quantum` is smaller. At 48 kHz a 128-frame quantum is 2.7 ms,
/// and scoring that as a hole would count ordinary scheduling jitter.
const GAP_FLOOR: Duration = Duration::from_millis(10);

/// Exclusive upper edges (ms). PLC hides ~50 ms; the drought fuse is two de-prime windows
/// (80–120 ms): `<20` inaudible, `<50` concealable, `<100` borderline, `≥100` a dropout.
pub const GAP_HIST_EDGES_MS: [u64; 3] = [20, 50, 100];

/// One reporting window of capture vitals.
///
/// Distinguishes a quiet host (`peak` ~0, no drops), a working stream (`peak` > 0), and a stream
/// we are damaging (`dropped_chunks` > 0). Without these, those three look identical in a log.
#[derive(Default)]
pub struct CaptureStats {
    pub frames: u64,
    /// Interleaved samples — RMS denominator. Using `frames` instead inflates RMS by
    /// `sqrt(channels)`, so a sine reports RMS equal to its peak.
    pub samples: u64,
    /// Separates a silent endpoint from a working one.
    pub peak: f32,
    /// Sum of squares for RMS. Far below peak means the endpoint is attenuated (20 % volume is
    /// ~14 dB before Opus).
    pub sumsq: f64,
    /// Chunks the encode thread did not take. The encoder concatenates across the hole: a click
    /// and a permanent shift of everything after it.
    pub dropped_chunks: u64,
    /// Callbacks that missed cadence. `delivered_pct` cannot say how: one 2 s hole and three
    /// hundred 8 ms hiccups are the same percentage and different faults.
    pub gaps: u64,
    /// Largest of those, µs. Logged in ms; stored in µs so a sub-ms threshold is expressible.
    pub max_gap_us: u64,
    /// Counts in [`GAP_HIST_EDGES_MS`] buckets. `gaps` + `max_gap_ms` cannot tell sixty 30 ms
    /// stalls from fifty-nine 12 ms hiccups and one outage.
    pub gap_hist: [u64; GAP_HIST_EDGES_MS.len() + 1],
    /// Audio those gaps cost. If this does not account for the `delivered_pct` shortfall, the
    /// remainder is sub-[`GAP_FLOOR`] loss.
    pub missing_us: u64,
    /// Callbacks that ran with nothing to dequeue. On-time empty is not the same as nobody feeding.
    pub missed_dequeues: u64,
    /// Spans spent away from `Streaming`. `gaps` cannot see these: a paused stream fires no
    /// callbacks, so the caller drops its cadence stamp. The reporting window still stretches
    /// by that time, which is why `delivered_pct` falls with `gaps=0`.
    pub pauses: u64,
    pub paused_us: u64,
}

impl CaptureStats {
    pub fn observe(&mut self, samples: &[f32], channels: u32) {
        self.frames += (samples.len() / channels.max(1) as usize) as u64;
        self.samples += samples.len() as u64;
        for &s in samples {
            let a = s.abs();
            if a > self.peak {
                self.peak = a;
            }
            self.sumsq += (s as f64) * (s as f64);
        }
    }

    /// `since_last` is `None` on the first callback and after a state transition: the caller
    /// drops its stamp across pause so a legitimate Paused span is not one enormous hole.
    /// [`Self::observe_pause`] records that span. `quantum` is the negotiated buffer duration,
    /// not the one we asked for — a 21.3 ms graph is not gapping at 21.3 ms cadence.
    pub fn observe_callback(&mut self, since_last: Option<Duration>, quantum: Duration) {
        let Some(delta) = since_last else { return };
        if delta > (quantum * 2).max(GAP_FLOOR) {
            // Missing audio, not the callback delta: one quantum of that delta is the buffer we
            // were handed. Reporting the delta would inflate every gap by the quantum and disagree
            // with the Windows feed, which sizes holes from the device position.
            self.observe_gap(delta.saturating_sub(quantum));
        }
    }

    /// Shared hole accounting: Linux callback cadence and the Windows discontinuity flag.
    pub fn observe_gap(&mut self, missing: Duration) {
        self.gaps += 1;
        let us = missing.as_micros() as u64;
        self.max_gap_us = self.max_gap_us.max(us);
        self.missing_us = self.missing_us.saturating_add(us);
        let ms = us / 1_000;
        let bucket = GAP_HIST_EDGES_MS
            .iter()
            .position(|&edge| ms < edge)
            .unwrap_or(GAP_HIST_EDGES_MS.len());
        self.gap_hist[bucket] += 1;
    }

    pub fn max_gap_ms(&self) -> u64 {
        self.max_gap_us / 1_000
    }

    pub fn missing_ms(&self) -> u64 {
        self.missing_us / 1_000
    }

    /// `a/b/c/d` = counts under 20 / 50 / 100 ms and ≥100 ms. One field so the line stays greppable.
    pub fn gap_hist(&self) -> String {
        self.gap_hist
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Record a span away from `Streaming`. Called on the transition back so the span lands in
    /// the same window whose `delivered_pct` it diluted.
    pub fn observe_pause(&mut self, span: Duration) {
        self.pauses += 1;
        self.paused_us += span.as_micros() as u64;
    }

    pub fn paused_ms(&self) -> u64 {
        self.paused_us / 1_000
    }

    /// Once [`STATS_EVERY`] has passed since `since`: log the window (with a warning when
    /// chunks were dropped) and start the next. `device` names the endpoint on a backend
    /// that picks one; `pauses` stays zero on a backend that never pauses.
    pub fn flush_window(&mut self, since: &mut Instant, rate_hz: u32, device: Option<&str>) {
        if since.elapsed() < STATS_EVERY {
            return;
        }
        let (peak_db, rms_db, delivered_pct) = self.summary(since.elapsed(), rate_hz);
        let device = device.map(tracing::field::display);
        if self.dropped_chunks > 0 {
            tracing::warn!(
                device,
                dropped_chunks = self.dropped_chunks,
                "the audio encode thread could not keep up — captured audio was DROPPED; the \
                 stream will click and everything after it shifts"
            );
        }
        tracing::info!(
            device,
            peak_db = format!("{peak_db:.1}"),
            rms_db = format!("{rms_db:.1}"),
            delivered_pct = format!("{delivered_pct:.0}"),
            // Shape of the `delivered_pct` shortfall: one long hole and three hundred short
            // ones share a percentage. Buckets under 20/50/100 ms and ≥ 100 ms.
            gaps = self.gaps,
            max_gap_ms = self.max_gap_ms(),
            gap_hist = %self.gap_hist(),
            missing_ms = self.missing_ms(),
            // Time the capture was not streaming. `gaps` can't see it.
            pauses = self.pauses,
            paused_ms = self.paused_ms(),
            missed_dequeues = self.missed_dequeues,
            dropped_chunks = self.dropped_chunks,
            "desktop audio capture"
        );
        *self = CaptureStats::default();
        *since = Instant::now();
    }

    /// `(peak dBFS, rms dBFS, delivered %)`. Silence is -120 dB, not -inf, so the log stays parseable.
    pub fn summary(&self, elapsed: Duration, sample_rate: u32) -> (f64, f64, f64) {
        let rms = (self.sumsq / (self.samples as f64).max(1.0)).sqrt();
        let db = |v: f64| if v > 0.0 { 20.0 * v.log10() } else { -120.0 };
        // Expected frames. A shortfall is the endpoint not delivering in real time; peak/RMS cannot show that.
        let expected = elapsed.as_secs_f64() * sample_rate as f64;
        (
            db(self.peak as f64),
            db(rms),
            (self.frames as f64 / expected.max(1.0)) * 100.0,
        )
    }
}

/// One reporting window of audio egress vitals.
///
/// Denominated in this session's frame, like [`InfillPolicy`] — see [`SendStats::new`]. Capture
/// holes and send slips are different claims; a clean egress line with capture holes moves the
/// search upstream.
pub struct SendStats {
    /// One protocol frame of this session, and the slip threshold: late by less than this is jitter.
    ///
    /// Carried, not read from Opus [`FRAME_MS`](punktfunk_core::audio::FRAME_MS). On a lossless
    /// plane pacing 1 ms frames, a 5 ms threshold would miss four-frame slips and still report
    /// `late=0`.
    frame: Duration,
    pub sent: u64,
    /// Frames synthesized to cover a capture hole. Wire continuity is not captured continuity.
    pub infilled: u64,
    pub late: u64,
    /// Worst miss, µs. Kept even at `late=0`: never late vs never late by a whole frame.
    pub max_late_us: u64,
    /// Widest gap between consecutive departures, µs. Client starvation is the wire going quiet.
    pub max_spacing_us: u64,
    /// Schedule fell more than `PACE_REANCHOR` behind and was re-anchored. Each one silently
    /// forgives accumulated debt.
    pub reanchors: u64,
}

impl SendStats {
    /// Window for a session pacing `frame_us` frames (`audio_frame_us` from Welcome, 5 000 on Opus).
    /// Same value as [`InfillPolicy`] and the encode-loop pacer.
    ///
    /// No `Default`: a zero frame makes `late >= self.frame` true for every departure, so a
    /// mistaken window would report 100 % slips. Windows are rebuilt on every flush.
    pub fn new(frame_us: u32) -> SendStats {
        SendStats {
            // Same floor as `InfillPolicy::new`.
            frame: Duration::from_micros(frame_us.max(1) as u64),
            sent: 0,
            infilled: 0,
            late: 0,
            max_late_us: 0,
            max_spacing_us: 0,
            reanchors: 0,
        }
    }

    /// `true` when this departure counted as late, so the session total scores it against the
    /// same threshold this window does rather than a second copy of it.
    pub fn observe_departure(
        &mut self,
        late: Duration,
        since_prev: Option<Duration>,
        infilled: bool,
    ) -> bool {
        self.sent += 1;
        if infilled {
            self.infilled += 1;
        }
        self.max_late_us = self.max_late_us.max(late.as_micros() as u64);
        // Inclusive: one whole frame of this session.
        let was_late = late >= self.frame;
        if was_late {
            self.late += 1;
        }
        if let Some(gap) = since_prev {
            self.max_spacing_us = self.max_spacing_us.max(gap.as_micros() as u64);
        }
        was_late
    }

    pub fn observe_reanchor(&mut self) {
        self.reanchors += 1;
    }

    pub fn max_late_ms(&self) -> u64 {
        self.max_late_us / 1_000
    }

    pub fn max_spacing_ms(&self) -> u64 {
        self.max_spacing_us / 1_000
    }
}

/// Wall-clock silence budget for one hole. Past this the desktop is quiet, not glitching, and
/// the wire stops. Not derived from the frame duration: 500 ms is 500 ms whether the plane
/// sends 100 or 500 frames into it. See `design/hi-res-audio.md`.
pub const INFILL_MAX: Duration = Duration::from_millis(500);

// No module-scope frame constant: `punktfunk_core::audio::FRAME_MS` is the Opus plane only.
// Infill threshold and egress slip live on [`InfillPolicy::new`] and [`SendStats::new`]. The
// lossless `0xD3` frame is whatever the handshake negotiated. Tests keep a local copy for the
// "5 ms session is unchanged" assertions.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Infill {
    Wait,
    Silence,
    /// The budget is spent. Next real chunk begins a new continuity.
    Quiet,
}

/// Whether, and for how long, the wire covers a capture hole with silence.
///
/// How long a capture-chunk size is remembered. A graph delivers its buffer every cycle, so
/// that size stays; one merged wake after a capture stall ages out instead of raising a
/// threshold for the rest of the session.
const CHUNK_MAX_WINDOW: Duration = Duration::from_secs(2);

/// Largest value noted over the last one to two [`CHUNK_MAX_WINDOW`]s. Time is passed in.
#[derive(Debug, Default)]
pub struct RecentMax<T> {
    cur: T,
    prev: T,
    since: Option<Instant>,
}

impl<T: Ord + Copy + Default> RecentMax<T> {
    pub fn note(&mut self, v: T, now: Instant) {
        let since = *self.since.get_or_insert(now);
        let age = now.duration_since(since);
        if age >= CHUNK_MAX_WINDOW {
            self.prev = if age >= CHUNK_MAX_WINDOW * 2 {
                T::default()
            } else {
                self.cur
            };
            self.cur = T::default();
            self.since = Some(now);
        }
        self.cur = self.cur.max(v);
    }

    pub fn get(&self) -> T {
        self.cur.max(self.prev)
    }
}

/// Silence on the session's frame schedule, with continuous `seq` and pts, keeps the client's
/// de-jitter ring fed so a hole costs only the audio that was missing, not a de-prime/re-prime.
/// Time is passed in. Denominated in the session's frame — see [`InfillPolicy::new`].
pub struct InfillPolicy {
    /// One protocol frame of this session. A 96/24 lossless session paces 1 ms frames; a policy
    /// written in 5 ms units would cover a fifth as long and spend the budget five times as fast.
    frame: Duration,
    /// Silence already sent for the open hole, in wall-clock time, not frames. A frame count
    /// made the same 500 ms budget a different amount of real time on every lossless rung.
    filled: Duration,
    broke: bool,
    /// Largest recent capture chunk; see [`Self::after`]. Zero until the caller reports one.
    quantum: RecentMax<Duration>,
}

impl InfillPolicy {
    /// `frame_us` is the session's resolved frame: `audio_frame_us` from Welcome, 5 000 on Opus —
    /// the same value the encode loop paces and stamps `pts_ns` with.
    ///
    /// `max(1)` floors a malformed plane. A zero frame would leave [`Self::decide`] unable to
    /// spend the budget, covering a hole with silence forever.
    pub fn new(frame_us: u32) -> InfillPolicy {
        InfillPolicy {
            frame: Duration::from_micros(frame_us.max(1) as u64),
            filled: Duration::ZERO,
            broke: false,
            quantum: RecentMax::default(),
        }
    }

    /// Keep the largest recent capture-chunk duration. A short buffer never lowers
    /// [`Self::after`]; a long one stops raising it once it ages out of [`RecentMax`].
    pub fn note_quantum(&mut self, chunk: Duration, now: Instant) {
        self.quantum.note(chunk, now);
    }

    /// How long a hole may run before the wire covers it.
    ///
    /// Two frames of this session: the client's ring is sized in its own frames. Never less than
    /// one chunk plus one frame, so a graph clamped to a 21 ms buffer (`min-quantum = 1024`) is
    /// not a hole when a chunk is a couple of milliseconds late.
    pub fn after(&self) -> Duration {
        (self.frame * 2).max(self.quantum.get() + self.frame)
    }

    /// Decide the slot due now. Call exactly once per due frame — it consumes budget.
    pub fn decide(&mut self, since_last_chunk: Duration) -> Infill {
        if since_last_chunk < self.after() {
            return Infill::Wait;
        }
        if self.filled >= INFILL_MAX {
            self.broke = true;
            return Infill::Quiet;
        }
        // One frame of silence costs one frame of budget. Nothing else here needs the frame length.
        self.filled += self.frame;
        Infill::Silence
    }

    /// Budget spent: the caller can block for real audio instead of waking to stay quiet.
    pub fn exhausted(&self) -> bool {
        self.filled >= INFILL_MAX
    }

    /// Silence sent for the open hole. After [`decide`](Self::decide) returns `Silence`, equal to
    /// one frame means this is the first of the hole (the fade); larger is plain silence.
    pub fn covered(&self) -> Duration {
        self.filled
    }

    pub fn frame(&self) -> Duration {
        self.frame
    }

    /// A real chunk arrived. `true` if the hole broke continuity: the redundancy predecessor
    /// and any partial frame straddling the hole must not be spliced onto what comes next.
    pub fn chunk_arrived(&mut self) -> bool {
        self.filled = Duration::ZERO;
        std::mem::take(&mut self.broke)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window logs once it is due, then starts empty.
    #[test]
    fn a_capture_window_flushes_once_due() {
        let mut s = CaptureStats::default();
        s.observe(&[0.5, -0.5], 2);
        s.dropped_chunks = 1;
        let mut since = Instant::now();
        s.flush_window(&mut since, 48_000, None);
        assert_eq!((s.frames, s.dropped_chunks), (1, 1), "flushed early");
        let Some(due) = Instant::now().checked_sub(STATS_EVERY) else {
            return;
        };
        since = due;
        s.flush_window(&mut since, 48_000, Some("Speakers"));
        assert_eq!((s.frames, s.dropped_chunks, s.peak), (0, 0, 0.0));
        assert!(since > due, "the next window starts now");
    }

    /// A dud default change every ~2 s must re-assert a few times, then concede, and warn once each.
    #[test]
    fn fight_damper_concedes_instead_of_looping_forever() {
        let t0 = Instant::now();
        let mut d = FightDamper::new(t0);
        let mut reasserts = 0;
        let (mut warns_fighting, mut warns_giving_up) = (0, 0);
        for i in 0..8 {
            d.observed_at(t0 + Duration::from_millis(i * 2_000));
            if d.should_reassert() {
                reasserts += 1;
                if d.warn_now() {
                    warns_fighting += 1;
                }
            } else if d.warn_giving_up() {
                warns_giving_up += 1;
            }
        }
        assert_eq!(
            reasserts, FIGHT_LIMIT,
            "must stop after the window's budget"
        );
        assert_eq!(warns_fighting, 1, "one warning per burst, not one per flip");
        assert_eq!(warns_giving_up, 1, "concede exactly once");
    }

    #[test]
    fn fight_damper_always_fixes_isolated_changes() {
        let t0 = Instant::now();
        let mut d = FightDamper::new(t0);
        let mut reasserts = 0;
        for i in 1..=10 {
            d.observed_at(t0 + FIGHT_WINDOW * i);
            if d.should_reassert() {
                reasserts += 1;
            }
        }
        assert_eq!(reasserts, 10, "isolated changes must always be corrected");
    }

    #[test]
    fn fight_damper_rearms_after_the_backoff() {
        let t0 = Instant::now();
        let mut d = FightDamper::new(t0);
        for i in 0..FIGHT_LIMIT + 2 {
            d.observed_at(t0 + Duration::from_millis(i as u64 * 500));
            d.should_reassert();
        }
        assert!(d.is_paused(), "should have conceded");
        d.observed_at(t0 + FIGHT_BACKOFF + FIGHT_WINDOW * 2);
        assert!(d.should_reassert(), "must re-arm once the backoff expires");
    }

    #[test]
    fn capture_stats_separate_silence_from_signal() {
        let mut quiet = CaptureStats::default();
        quiet.observe(&[0.0; 480], 2);
        let (peak, rms, _) = quiet.summary(Duration::from_secs(1), 48_000);
        assert_eq!(peak, -120.0, "digital silence reports the floor, not -inf");
        assert_eq!(rms, -120.0);

        let mut loud = CaptureStats::default();
        let tone: Vec<f32> = (0..480).map(|i| (i as f32 * 0.13).sin() * 0.5).collect();
        loud.observe(&tone, 2);
        assert_eq!(
            loud.frames, 240,
            "480 interleaved stereo samples = 240 frames"
        );
        let (peak, rms, _) = loud.summary(Duration::from_secs(1), 48_000);
        assert!(
            peak > -8.0 && peak <= 0.0,
            "peak {peak} dBFS should track a 0.5 tone"
        );
        // A sine's RMS is amplitude / sqrt(2) ≈ 3 dB below peak. Equal to peak is a
        // frames-vs-samples mix-up in the denominator.
        assert!(
            rms < peak - 2.0,
            "RMS {rms} vs peak {peak}: a sine must sit ~3 dB below its peak"
        );
    }

    #[test]
    fn capture_stats_report_a_delivery_shortfall() {
        let mut full = CaptureStats::default();
        full.observe(&vec![0.1f32; 48_000 * 2], 2);
        let (_, _, pct) = full.summary(Duration::from_secs(1), 48_000);
        assert!((pct - 100.0).abs() < 1.0, "expected ~100 %, got {pct}");

        let mut half = CaptureStats::default();
        half.observe(&vec![0.1f32; 48_000], 2);
        let (_, _, pct) = half.summary(Duration::from_secs(1), 48_000);
        assert!((pct - 50.0).abs() < 1.0, "expected ~50 %, got {pct}");
    }

    /// 5 ms quantum we ask for — the cadence the gap tests measure against.
    const Q: Duration = Duration::from_millis(5);

    /// `delivered_pct` is the same for one 2 s hole and for three hundred 8 ms hiccups; the
    /// counters must separate those without a second log.
    #[test]
    fn gap_accounting_tells_one_long_hole_from_many_short_ones() {
        let mut one = CaptureStats::default();
        one.observe_callback(None, Q);
        one.observe_callback(Some(Duration::from_secs(2)), Q);
        assert_eq!(one.gaps, 1);
        // Two seconds between callbacks, one quantum of which was audio we were handed.
        assert_eq!(one.max_gap_ms(), 2_000 - Q.as_millis() as u64);

        // Three hundred 8 ms holes, each arriving as a 13 ms callback delta (8 + 5 ms quantum).
        let mut many = CaptureStats::default();
        for _ in 0..300 {
            many.observe_callback(Some(Q + Duration::from_millis(8)), Q);
        }
        assert_eq!(many.gaps, 300);
        assert_eq!(many.max_gap_ms(), 8);

        assert!(
            one.max_gap_ms() > many.max_gap_ms() * 100,
            "the discriminator is the SHAPE, not the total"
        );
    }

    /// `gaps=60` either way; the histogram is the shape.
    #[test]
    fn gap_histogram_gives_the_shape_and_the_cost() {
        let mut stalls = CaptureStats::default();
        for _ in 0..60 {
            // A 30 ms hole arrives as a 35 ms callback delta at the 5 ms quantum.
            stalls.observe_callback(Some(Q + Duration::from_millis(30)), Q);
        }
        assert_eq!(stalls.gaps, 60);
        assert_eq!(stalls.gap_hist(), "0/60/0/0", "all in the <50 ms bucket");
        assert_eq!(stalls.missing_ms(), 60 * 30);

        let mut mixed = CaptureStats::default();
        for _ in 0..59 {
            mixed.observe_callback(Some(Q + Duration::from_millis(12)), Q);
        }
        // The Windows feed measures the hole from the device position and calls this directly.
        mixed.observe_gap(Duration::from_millis(1_092));
        assert_eq!(mixed.gaps, 60, "same count as the stalls above…");
        assert_eq!(
            mixed.gap_hist(),
            "59/0/0/1",
            "…and nothing like the same shape"
        );
        assert_eq!(mixed.missing_ms(), 59 * 12 + 1_092);
        assert_eq!(mixed.max_gap_ms(), 1_092);

        // Edges are exclusive on the upper side: exactly 20 ms is the second bucket.
        let mut edge = CaptureStats::default();
        edge.observe_gap(Duration::from_millis(20));
        edge.observe_gap(Duration::from_micros(19_999));
        edge.observe_gap(Duration::from_millis(100));
        assert_eq!(edge.gap_hist(), "1/1/0/1");
    }

    /// A graph delivering exactly the negotiated quantum is not gapping, including a 21.3 ms VM clamp.
    #[test]
    fn a_negotiated_cadence_is_never_a_gap() {
        let mut s = CaptureStats::default();
        for _ in 0..100 {
            s.observe_callback(Some(Duration::from_micros(5_100)), Q); // 5 ms + jitter
        }
        assert_eq!(s.gaps, 0, "ordinary jitter at the negotiated quantum");

        let clamped = Duration::from_micros(21_333); // 1024 frames @ 48 kHz
        let mut vm = CaptureStats::default();
        for _ in 0..100 {
            vm.observe_callback(Some(clamped), clamped);
        }
        assert_eq!(vm.gaps, 0, "a clamped quantum is slow, not gapping");
        // A real hole on that same graph still scores.
        vm.observe_callback(Some(Duration::from_millis(200)), clamped);
        assert_eq!(vm.gaps, 1);
    }

    /// Opus `0xC9` 5 ms, local to these tests. Production policy is the session frame, not this constant.
    const FRAME_MS: u32 = punktfunk_core::audio::FRAME_MS;
    const OPUS_FRAME_US: u32 = FRAME_MS * 1_000;

    fn cover_a_hole(p: &mut InfillPolicy, frame: Duration) -> usize {
        let mut silence = 0usize;
        let mut open = p.after();
        loop {
            match p.decide(open) {
                Infill::Silence => {
                    silence += 1;
                    open += frame;
                }
                Infill::Quiet => return silence,
                Infill::Wait => {
                    unreachable!("the hole is open — {open:?} is past the infill threshold")
                }
            }
            assert!(silence < 10_000, "the budget must be finite");
        }
    }

    /// Cover exactly the budget, then stop. Without the first a short hole de-primes the client;
    /// without the second an idle desktop pays for silence forever.
    #[test]
    fn infill_covers_a_hole_and_then_admits_the_host_is_quiet() {
        let frame = Duration::from_millis(FRAME_MS as u64);
        let mut p = InfillPolicy::new(OPUS_FRAME_US);
        // Ordinary quantum jitter, not a hole — nothing owed.
        assert_eq!(p.decide(Duration::ZERO), Infill::Wait);
        assert_eq!(p.decide(p.after() - Duration::from_millis(1)), Infill::Wait);

        let silence = cover_a_hole(&mut p, frame);
        assert_eq!(
            silence as u64 * FRAME_MS as u64,
            INFILL_MAX.as_millis() as u64,
            "the wire must cover exactly the budget, in frames of {FRAME_MS} ms"
        );
        assert!(p.exhausted(), "…and then stop asking");
    }

    /// A graph's buffer arrives every cycle and stays remembered; one merged capture wake after
    /// a stall ages out, where a session-long max kept the bonus drain off for good.
    #[test]
    fn a_one_off_chunk_ages_out_of_the_recent_max() {
        let t0 = Instant::now();
        let step = Duration::from_millis(10);
        let mut m = RecentMax::default();
        m.note(10usize, t0);
        m.note(60, t0 + step);
        assert_eq!(m.get(), 60);
        let mut t = t0 + step;
        for _ in 0..500 {
            t += step;
            m.note(10, t);
        }
        assert_eq!(m.get(), 10, "5 s of 10-sample chunks");
        for _ in 0..1_000 {
            t += step;
            m.note(1_024, t);
        }
        assert_eq!(m.get(), 1_024, "a steady large quantum is not a spike");
        // Idle past two windows: the next chunk alone is the max.
        m.note(10, t + CHUNK_MAX_WINDOW * 2);
        assert_eq!(m.get(), 10);
    }

    /// Clamped quantum lifts the threshold to one chunk plus one frame. `covered()` equal to
    /// `frame()` is the first frame of a hole (the fade).
    #[test]
    fn infill_threshold_follows_a_clamped_quantum() {
        let frame = Duration::from_millis(FRAME_MS as u64);
        let t = Instant::now();
        let mut tight = InfillPolicy::new(OPUS_FRAME_US);
        tight.note_quantum(Duration::from_micros(2_667), t); // 128 frames at 48 kHz
        tight.note_quantum(frame, t); // 240 frames — what we ask for
        assert_eq!(
            tight.after(),
            frame * 2,
            "a frame-sized quantum changes nothing"
        );

        let mut vm = InfillPolicy::new(OPUS_FRAME_US);
        vm.note_quantum(Duration::from_micros(21_333), t); // 1024 frames at 48 kHz
        vm.note_quantum(Duration::from_micros(2_667), t); // one short buffer never lowers it
        assert_eq!(
            vm.after(),
            Duration::from_micros(26_333),
            "one chunk plus one frame"
        );
        assert_eq!(
            vm.decide(Duration::from_millis(23)),
            Infill::Wait,
            "a chunk 2 ms late on a 21 ms quantum is not a hole"
        );
        assert_eq!(vm.decide(Duration::from_millis(27)), Infill::Silence);
        assert_eq!(vm.covered(), vm.frame(), "the first frame of the hole");
        assert_eq!(vm.decide(Duration::from_millis(32)), Infill::Silence);
        assert_eq!(vm.covered(), vm.frame() * 2, "…and no longer the first");
        vm.chunk_arrived();
        assert_eq!(vm.covered(), Duration::ZERO, "a chunk closes the hole");
    }

    /// Pin the Opus 5 ms session: 10 ms infill threshold, 100 frames of cover, 5 ms slip.
    /// A lossless-plane change must not retune the plane every shipping client uses.
    #[test]
    fn a_five_millisecond_opus_session_is_unchanged() {
        let mut p = InfillPolicy::new(OPUS_FRAME_US);
        assert_eq!(p.after(), Duration::from_millis(10), "two 5 ms frames");
        assert_eq!(
            cover_a_hole(&mut p, Duration::from_millis(FRAME_MS as u64)),
            100,
            "500 ms of budget in 5 ms frames"
        );

        // Both sides of the slip boundary: "4.999 ms is not late" also passes for infinity, and
        // "5 ms is late" also passes for zero.
        let mut s = SendStats::new(OPUS_FRAME_US);
        s.observe_departure(Duration::from_micros(4_999), None, false);
        assert_eq!(s.late, 0, "just under a 5 ms frame is jitter");
        s.observe_departure(Duration::from_millis(5), None, false);
        assert_eq!(s.late, 1, "one whole 5 ms frame is a slipped slot");
    }

    #[test]
    fn every_lossless_frame_length_gets_the_same_wall_clock_budget() {
        for frame_us in punktfunk_core::audio::pcm::FRAME_US_LADDER {
            let frame = Duration::from_micros(frame_us as u64);
            let mut p = InfillPolicy::new(frame_us);
            assert_eq!(p.after(), frame * 2, "{frame_us} µs: two of its own frames");
            let silence = cover_a_hole(&mut p, frame);
            // Covered to within one frame: a frame is atomic, so 3 ms → 167 frames = 501 ms
            // rather than stopping short. The 5 ms Opus rung divides 500 ms exactly.
            let covered = frame * silence as u32;
            assert!(
                covered >= INFILL_MAX && covered < INFILL_MAX + frame,
                "{frame_us} µs covered {silence} frames = {covered:?}, want {INFILL_MAX:?} \
                 rounded up by under one frame"
            );
        }
    }

    #[test]
    fn a_flowing_stream_never_infills() {
        let mut p = InfillPolicy::new(OPUS_FRAME_US);
        for _ in 0..1_000 {
            assert_eq!(
                p.decide(Duration::from_millis(FRAME_MS as u64)),
                Infill::Wait
            );
        }
        assert!(!p.exhausted());
        assert!(!p.chunk_arrived(), "no hole means no discontinuity");
    }

    /// Across a covered hole `seq` and pts never broke, so the redundancy predecessor is still valid.
    #[test]
    fn only_an_uncovered_hole_breaks_continuity() {
        let frame = Duration::from_millis(FRAME_MS as u64);
        let mut covered = InfillPolicy::new(OPUS_FRAME_US);
        for k in 0..20u32 {
            covered.decide(covered.after() + frame * k);
        }
        assert!(
            !covered.chunk_arrived(),
            "a covered hole is continuous — the client heard silence, not a splice"
        );

        let mut lost = InfillPolicy::new(OPUS_FRAME_US);
        cover_a_hole(&mut lost, frame);
        assert!(
            lost.chunk_arrived(),
            "past the budget the wire went quiet, so nothing before the hole may be spliced on"
        );
        // Next hole starts from a clean budget rather than an exhausted one.
        assert!(!lost.exhausted());
        assert_eq!(cover_a_hole(&mut lost, frame) as u64 * FRAME_MS as u64, 500);
    }

    /// Dropping the stamp across a pause is not a gap. `observe_pause` still
    /// counts it for the log line.
    #[test]
    fn a_paused_span_is_not_a_gap_and_is_still_counted() {
        let mut s = CaptureStats::default();
        s.observe_callback(Some(Duration::from_millis(5)), Q);
        s.observe_callback(None, Q); // stamp dropped; the pause spanned an unknowable time
        s.observe_callback(Some(Duration::from_millis(5)), Q);
        assert_eq!(s.gaps, 0);
        assert_eq!(s.max_gap_ms(), 0);
        assert_eq!(s.pauses, 0);

        s.observe_pause(Duration::from_millis(16_214));
        s.observe_callback(None, Q);
        s.observe_callback(Some(Duration::from_millis(5)), Q);
        assert_eq!(s.gaps, 0, "a pause is still not a delivery gap");
        assert_eq!(s.max_gap_ms(), 0);
        assert_eq!(s.pauses, 1, "…but it is now countable");
        assert_eq!(s.paused_ms(), 16_214);
    }

    #[test]
    fn pause_spans_accumulate_and_stay_countable() {
        let mut long = CaptureStats::default();
        long.observe_pause(Duration::from_millis(38_400));

        let mut flappy = CaptureStats::default();
        for ms in [12_534, 17_030, 8_765] {
            flappy.observe_pause(Duration::from_millis(ms));
        }

        assert_eq!(long.pauses, 1);
        assert_eq!(flappy.pauses, 3);
        assert_eq!(flappy.paused_ms(), 38_329);
        assert!(
            long.paused_ms().abs_diff(flappy.paused_ms()) < 100,
            "near-identical dead time, and the count is the only thing that separates them"
        );
    }

    #[test]
    fn starvation_and_absence_are_told_apart() {
        let mut starved = CaptureStats::default();
        for _ in 0..60 {
            starved.observe_callback(Some(Duration::from_millis(30)), Q);
        }

        let mut absent = CaptureStats::default();
        absent.observe_pause(Duration::from_millis(1_800));

        assert_eq!(starved.gaps, 60);
        assert_eq!(starved.pauses, 0, "a running stream was never absent");
        assert_eq!(absent.gaps, 0);
        assert_eq!(absent.pauses, 1, "an absent stream never got to be slow");
        assert_eq!(absent.paused_ms(), 1_800);
    }

    #[test]
    fn a_healthy_pacer_reports_nothing_alarming() {
        let mut s = SendStats::new(OPUS_FRAME_US);
        let frame = Duration::from_millis(FRAME_MS as u64);
        for i in 0..200 {
            s.observe_departure(Duration::ZERO, (i > 0).then_some(frame), false);
        }
        assert_eq!(s.sent, 200);
        assert_eq!(s.late, 0);
        assert_eq!(s.reanchors, 0);
        assert_eq!(s.infilled, 0);
        assert_eq!(s.max_late_ms(), 0);
        assert_eq!(s.max_spacing_ms(), FRAME_MS as u64);
    }

    #[test]
    fn a_slipped_slot_is_one_frame_of_whatever_this_session_paces() {
        for frame_us in punktfunk_core::audio::pcm::FRAME_US_LADDER {
            let frame = Duration::from_micros(frame_us as u64);
            let mut s = SendStats::new(frame_us);
            // One microsecond under a frame is jitter; the frame itself is a slip — inclusive.
            assert!(
                !s.observe_departure(frame - Duration::from_micros(1), None, false),
                "a hair under a frame is jitter, and the session total must agree"
            );
            assert_eq!(s.late, 0, "{frame_us} µs: sub-frame lateness is jitter");
            assert!(
                s.observe_departure(frame, None, false),
                "one whole frame is late, inclusive"
            );
            assert_eq!(
                s.late, 1,
                "{frame_us} µs: one whole frame is a slipped slot"
            );
            // Both were measured, so "never late" and "never late by a whole frame" differ.
            assert_eq!(s.max_late_us, frame.as_micros() as u64);
        }
    }

    #[test]
    fn a_slipped_slot_is_counted_and_its_worst_case_kept() {
        let mut s = SendStats::new(OPUS_FRAME_US);
        s.observe_departure(Duration::from_micros(3_400), None, false);
        assert_eq!(s.late, 0, "3.4 ms has not slipped a whole 5 ms slot");
        assert_eq!(s.max_late_ms(), 3, "…and it is still on the record");
        s.observe_departure(Duration::from_millis(6), None, false);
        s.observe_departure(
            Duration::from_millis(41),
            Some(Duration::from_millis(47)),
            false,
        );
        s.observe_departure(Duration::ZERO, Some(Duration::from_millis(5)), false);
        s.observe_reanchor();

        assert_eq!(s.late, 2);
        assert_eq!(s.max_late_ms(), 41);
        assert_eq!(s.max_spacing_ms(), 47, "the wire's worst quiet stretch");
        assert_eq!(s.reanchors, 1);
    }

    #[test]
    fn synthesized_frames_stay_distinguishable_from_captured_ones() {
        let mut s = SendStats::new(OPUS_FRAME_US);
        let frame = Duration::from_millis(FRAME_MS as u64);
        for _ in 0..100 {
            s.observe_departure(Duration::ZERO, Some(frame), true);
        }
        assert_eq!(s.sent, 100);
        assert_eq!(
            s.infilled, 100,
            "every one of these was silence we invented"
        );
        assert_eq!(s.late, 0);
    }

    /// The ask is a count (sessions overlap); every guard drop hands the devices back. This test
    /// is the static's only toucher, so it owns the counter for its run.
    #[test]
    fn keep_host_audio_guard_counts_overlapping_sessions() {
        assert!(!session_keeps_default());
        let a = keep_host_audio_guard();
        assert!(session_keeps_default());
        let b = keep_host_audio_guard();
        drop(a);
        assert!(
            session_keeps_default(),
            "the second session still holds the ask"
        );
        drop(b);
        assert!(!session_keeps_default());
    }
}
