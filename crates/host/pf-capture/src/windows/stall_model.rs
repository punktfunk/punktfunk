//! Capture-stall model for IDD-push: holes in DWM frame delivery while
//! the desktop was composing.
//!
//! [`StallWatch`] gates on recent active flow, names each hole from the two
//! clocks the driver stamps — the drain heartbeat, and the pool taking a frame
//! — and feeds a metronome so periodic stalls self-diagnose. Damage-idle holes
//! (the cursor never moved) are real delivery gaps but stay out of the beat.
//!
//! Pure over `Instant`s and the driver's telemetry, so its tests run on every target.
//! `idd_push/stall.rs` writes the report, which reads the OS display events.

// Off Windows only the tests read this module.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::time::{Duration, Instant};

/// A hole in DWM delivery that opened after recent active compose ([`StallWatch`]).
///
/// The metronome is not fed here. [`StallWatch::report`] feeds it after the
/// verdict, so a damage-idle hole never advances the display-hardware beat.
pub(crate) struct Stall {
    pub(crate) gap: Duration,
}

/// One degraded stretch, closed by [`StallWatch::take_recovery`].
///
/// Per-hole stall lines gate on prior active flow, so a sustained slow phase
/// logs only the first hole; this summary is the stretch's remaining line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Recovery {
    pub(crate) degraded: Duration,
    /// Stall-sized holes (≥ [`StallWatch::STALL_MIN`]).
    pub(crate) holes: u32,
    pub(crate) hole_time: Duration,
    pub(crate) worst: Duration,
    /// Present→arrival over the stretch's frames (ms): the access unit's OS
    /// present stamp against the moment the host took it. `arrival_n` counts
    /// the frames that carried a stamp; 0 means none did, not a zero delay.
    pub(crate) arrival_last_ms: u64,
    pub(crate) arrival_mean_ms: u64,
    pub(crate) arrival_max_ms: u64,
    pub(crate) arrival_n: u32,
}

impl Recovery {
    /// `last/mean/max` present→arrival ms for the log line; `None` when no frame
    /// in the stretch carried an OS present stamp.
    pub(crate) fn arrival_ms(&self) -> Option<String> {
        (self.arrival_n > 0).then(|| {
            format!(
                "{}/{}/{}",
                self.arrival_last_ms, self.arrival_mean_ms, self.arrival_max_ms
            )
        })
    }
}

/// Open degraded stretch; closed into [`Recovery`].
struct Episode {
    started: Instant,
    last_hole_end: Instant,
    holes: u32,
    hole_time: Duration,
    worst: Duration,
    /// Present→arrival tally over the frames consumed inside the stretch.
    arrival_last_ms: u64,
    arrival_max_ms: u64,
    arrival_sum_ms: u64,
    arrival_n: u32,
}

/// Driver telemetry for one stall window (the AU section header), sampled
/// between the last pre-gap frame and the frame that ended the stall.
pub(crate) struct StallEvidence {
    /// Max `now − drain heartbeat` over the window, in milliseconds. `None` before the
    /// encoder is open, when the driver reports nothing.
    pub(crate) max_heartbeat_age_ms: Option<u64>,
    pub(crate) probes: Option<ProbeWindow>,
    /// DxgKrnl DDI summary for the window. `None` when the ETW session is unavailable.
    pub(crate) etw: Option<String>,
    /// Present-vs-queue counts (`EtwWatch::window_report`). Presents flowing
    /// while the queue starves = OS dropped composed frames; both silent =
    /// content stopped. `None` when the ETW session is unavailable.
    pub(crate) etw_counts: Option<EtwWindowCounts>,
    /// Cursor travel during the hole (px, |dx|+|dy|). `Some(0)` = nothing to
    /// compose (damage-idle), also under a declared hardware cursor, whose
    /// travel composes nothing; `Some(n>0)` = damage existed and DWM composed
    /// none of it. `None` = never sampled. The stall-ending frame's own move
    /// is not counted (capturer fold-on-next-call sampler).
    pub(crate) cursor_moved_px: Option<u32>,
}

/// Per-leg maxima from `probes::ProbeEngine::window` across one stall.
/// Every field is `None` when that probe is absent; absence is never guessed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ProbeWindow {
    pub(crate) fence_max_us: Option<u64>,
    /// Longest span with no `DwmGetCompositionTimingInfo` `cRefresh` advance (µs).
    pub(crate) dwm_tick_frozen_us: Option<u64>,
    /// Longest span with no `cFrame` advance (µs). Advisory only: on Win11
    /// `DWM_TIMING_INFO.cFrame` is refresh-synthesized and ticks without composes.
    pub(crate) dwm_frame_frozen_us: Option<u64>,
    pub(crate) dwm_flush_max_us: Option<u64>,
    /// Worst `D3DKMTGetScanLine` call latency (µs). Blocking here convicts the KMD.
    pub(crate) scanline_max_us: Option<u64>,
    /// Physical head present. Exclusive topology leaves only our IDD; latency
    /// still counts, scanline values do not.
    pub(crate) scanline_physical: bool,
    /// Worst high-res sleeper overshoot (µs). DPC-storm / CPU-starvation discriminator.
    pub(crate) cpu_max_overshoot_us: Option<u64>,
}

impl std::fmt::Display for ProbeWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = |v: Option<u64>| match v {
            Some(us) => format!("{:.0}ms", us as f64 / 1_000.0),
            None => "absent".to_string(),
        };
        write!(
            f,
            "fence={} dwm_tick_frozen={} dwm_frames_frozen={} dwm_flush={} scanline={}({}) \
             cpu_overshoot={}",
            ms(self.fence_max_us),
            ms(self.dwm_tick_frozen_us),
            ms(self.dwm_frame_frozen_us),
            ms(self.dwm_flush_max_us),
            ms(self.scanline_max_us),
            if self.scanline_physical {
                "physical"
            } else {
                "virtual"
            },
            ms(self.cpu_max_overshoot_us),
        )
    }
}

/// What one hole is, from the driver's own clocks. Probes and ETW ride the
/// report as evidence; they no longer name a class of their own.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum StallVerdict {
    /// No driver telemetry yet: host observation only.
    NoTelemetry,
    /// Drain heartbeat silent for a large share of the hole (CPU/MMCSS or dead WUDFHost).
    WorkerStalled,
    /// The worker kept draining and the pool took nothing: DWM composed nothing while
    /// something on the desktop was dirty.
    ComposeSilence,
    /// Compose silence with no cursor damage (it never moved, or a hardware cursor carried
    /// it): nothing was dirty. An input pause, not a display stall — kept out of the
    /// metronome and both repeated-stall warns.
    DamageIdle,
}

impl std::fmt::Display for StallVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoTelemetry => "no driver telemetry yet (no verdict)",
            Self::WorkerStalled => "driver-worker-stalled (heartbeat silent) — host CPU/MMCSS or a dead WUDFHost, NOT the display path",
            Self::ComposeSilence => "compose-silence (the driver's pool took no frame) — DWM composed nothing; the disturbance is below capture",
            Self::DamageIdle => "damage-idle (no cursor damage through the hole) — nothing was dirty, so DWM correctly composed nothing; an input pause, not a display stall",
        })
    }
}

/// Fold one stall window into a [`StallVerdict`] from the driver's clocks.
///
/// Heartbeat ever `max(gap/2, 250 ms)` stale convicts the worker (cadence is
/// ≤16 ms, so 250 ms is starvation; gap/2 scales long holes). A heartbeat that
/// stayed fresh acquits it, and a cursor that never moved says nothing was
/// dirty — with the ETW leg on, its counts must agree the flow was dwm-only,
/// so a game presenting through the hole is never demoted.
pub(crate) fn attribute(gap: Duration, evidence: &StallEvidence) -> StallVerdict {
    let Some(hb_age_ms) = evidence.max_heartbeat_age_ms else {
        return StallVerdict::NoTelemetry;
    };
    let gap_ms = gap.as_millis() as u64;
    if hb_age_ms >= (gap_ms / 2).max(250) {
        return StallVerdict::WorkerStalled;
    }
    let dwm_only = evidence.etw_counts.is_none_or(|c| c.flow_dwm_only);
    if evidence.cursor_moved_px == Some(0) && dwm_only {
        StallVerdict::DamageIdle
    } else {
        StallVerdict::ComposeSilence
    }
}

/// Capture-stall watch. A hole counts only when [`Self::RECENT`] pre-gap
/// frames all fit in [`Self::ACTIVE_SPAN`] — an idle desktop goes quiet
/// with no damage.
///
/// Reported stalls feed a [`pf_frame::metronome::Metronome`] after the verdict
/// so periodic DWM holes self-diagnose. Encode/network causes stay with the
/// recovery-cadence detector. [`Self::report`], in `idd_push/stall.rs`, logs.
pub(crate) struct StallWatch {
    /// Last [`Self::RECENT`] fresh-frame instants — activity-gate history.
    recent: std::collections::VecDeque<Instant>,
    cadence: pf_frame::metronome::Metronome,
    /// Session stall count and how many carried a coinciding OS display event.
    pub(crate) seen: u32,
    pub(crate) with_os_events: u32,
    /// Per-verdict counts in [`StallVerdict`] order. The metronomic WARN prints
    /// the session, not just the stall that tripped the beat.
    pub(crate) verdicts: [u32; 4],
    /// Open stretch; every stall-sized hole feeds it until sustained flow returns.
    episode: Option<Episode>,
    pending_recovery: Option<Recovery>,
    /// Reported-stall instants in [`Self::RATE_WINDOW`]. The metronome needs a
    /// stable period; this arm covers aperiodic bursts.
    rate_window: std::collections::VecDeque<Instant>,
    /// Last rate-arm WARN; spacing is [`Self::RATE_REWARN`].
    last_rate_warn: Option<Instant>,
}

impl StallWatch {
    /// Pre-gap frames that must be tight for active flow. Stalls are then spaced
    /// ≥ this many frame times — no extra log rate limit.
    const RECENT: usize = 8;
    /// Span the RECENT pre-gap frames must fit. 8 frames in 400 ms is 7 intervals
    /// ≈ 17.5 fps: a 30 fps-capped game passes; idle-desktop damage does not.
    const ACTIVE_SPAN: Duration = Duration::from_millis(400);
    /// Smallest stall hole. ~9 missed frames at 60 Hz; above encode/present jitter.
    const STALL_MIN: Duration = Duration::from_millis(150);
    /// Hole that is a content stop, not a stretch. Closes an open episode first so
    /// a quit-to-idle pause never folds into the tally.
    const EPISODE_BREAK: Duration = Duration::from_secs(10);
    /// Below this, the episode dissolves; the single stall's report already covers it.
    const EPISODE_MIN_HOLES: u32 = 2;
    /// Window for the aperiodic repeated-stall WARN.
    const RATE_WINDOW: Duration = Duration::from_secs(60);
    /// Stalls in [`Self::RATE_WINDOW`] that trip the rate WARN. Above a busy
    /// desktop's ~1/min; under a degraded session's dozens.
    const RATE_MIN_STALLS: usize = 3;
    /// Rate-arm re-WARN spacing. Metronomic arms pace via the metronome.
    const RATE_REWARN: Duration = Duration::from_secs(300);

    pub(crate) fn new() -> Self {
        Self {
            recent: std::collections::VecDeque::with_capacity(Self::RECENT + 1),
            cadence: pf_frame::metronome::Metronome::new(),
            seen: 0,
            with_os_events: 0,
            verdicts: [0; 4],
            episode: None,
            pending_recovery: None,
            rate_window: std::collections::VecDeque::new(),
            last_rate_warn: None,
        }
    }

    /// Record a reported stall at `now`. `Some(count)` when the rate WARN is due
    /// (≥ [`Self::RATE_MIN_STALLS`] in [`Self::RATE_WINDOW`], [`Self::RATE_REWARN`]
    /// spacing).
    pub(crate) fn note_for_rate_warn(&mut self, now: Instant) -> Option<usize> {
        self.rate_window.push_back(now);
        while let Some(front) = self.rate_window.front() {
            if now.duration_since(*front) > Self::RATE_WINDOW {
                self.rate_window.pop_front();
            } else {
                break;
            }
        }
        if self.rate_window.len() < Self::RATE_MIN_STALLS {
            return None;
        }
        if self
            .last_rate_warn
            .is_some_and(|t| now.duration_since(t) < Self::RATE_REWARN)
        {
            return None;
        }
        self.last_rate_warn = Some(now);
        Some(self.rate_window.len())
    }

    /// Per-verdict log token, indexed in [`StallVerdict`] declaration order.
    pub(crate) fn verdict_tally(&self) -> String {
        format!(
            "worker-stalled {}, compose-silence {}, damage-idle {}, no-telemetry {}",
            self.verdicts[1], self.verdicts[2], self.verdicts[3], self.verdicts[0]
        )
    }

    /// Drop flow history. A presentation-restart gap is self-inflicted; without this the
    /// first frame after it reads as a stall. Open episodes still close — those holes
    /// predate the restart.
    pub(crate) fn reset(&mut self) {
        self.recent.clear();
        self.close_episode();
    }

    /// Close the open episode into [`Self::pending_recovery`] if past the noise bar.
    /// The present→arrival tally collapses to last/mean/max here; a mean over an
    /// empty tally would divide by zero, so it stays 0 with `arrival_n` at 0.
    fn close_episode(&mut self) {
        if let Some(ep) = self.episode.take() {
            if ep.holes >= Self::EPISODE_MIN_HOLES {
                self.pending_recovery = Some(Recovery {
                    degraded: ep.last_hole_end.duration_since(ep.started),
                    holes: ep.holes,
                    hole_time: ep.hole_time,
                    worst: ep.worst,
                    arrival_last_ms: ep.arrival_last_ms,
                    arrival_mean_ms: ep
                        .arrival_sum_ms
                        .checked_div(u64::from(ep.arrival_n))
                        .unwrap_or(0),
                    arrival_max_ms: ep.arrival_max_ms,
                    arrival_n: ep.arrival_n,
                });
            }
        }
    }

    /// Take a closed stretch if one is waiting. Call after every
    /// [`Self::note_fresh`] / [`Self::reset`]: closure rides a non-stall frame.
    pub(crate) fn take_recovery(&mut self) -> Option<Recovery> {
        self.pending_recovery.take()
    }

    /// Record a fresh driver frame at `now`, with its present→arrival in ms if the access unit
    /// carried an OS present stamp. `Some` iff the frame ended a stall.
    pub(crate) fn note_fresh(&mut self, now: Instant, arrival_ms: Option<u64>) -> Option<Stall> {
        let was_active = self.recent.len() == Self::RECENT
            && self
                .recent
                .back()
                .zip(self.recent.front())
                .is_some_and(|(b, f)| b.duration_since(*f) <= Self::ACTIVE_SPAN);
        let gap = self.recent.back().map(|last| now.duration_since(*last));
        self.recent.push_back(now);
        if self.recent.len() > Self::RECENT {
            self.recent.pop_front();
        }
        let gap = gap?;
        if let (Some(ep), Some(ms)) = (self.episode.as_mut(), arrival_ms) {
            ep.arrival_last_ms = ms;
            ep.arrival_max_ms = ep.arrival_max_ms.max(ms);
            ep.arrival_sum_ms = ep.arrival_sum_ms.saturating_add(ms);
            ep.arrival_n += 1;
        }
        if gap >= Self::EPISODE_BREAK {
            // Content stopped (quit / long idle). Summarize the stretch; do not
            // fold a legitimate pause into the tally.
            self.close_episode();
        }
        if gap >= Self::STALL_MIN {
            match &mut self.episode {
                // Accumulate every stall-sized hole. The activity gate below
                // quiets per-hole reports (pre-gap spans the slow frames).
                Some(ep) => {
                    ep.holes += 1;
                    ep.hole_time += gap;
                    ep.worst = ep.worst.max(gap);
                    ep.last_hole_end = now;
                }
                None if was_active => {
                    self.episode = Some(Episode {
                        started: now - gap,
                        last_hole_end: now,
                        holes: 1,
                        hole_time: gap,
                        worst: gap,
                        arrival_last_ms: arrival_ms.unwrap_or(0),
                        arrival_max_ms: arrival_ms.unwrap_or(0),
                        arrival_sum_ms: arrival_ms.unwrap_or(0),
                        arrival_n: u32::from(arrival_ms.is_some()),
                    });
                }
                None => {}
            }
        } else if was_active {
            // [`Self::RECENT`] tight frames: the stretch is over.
            self.close_episode();
        }
        if !was_active || gap < Self::STALL_MIN {
            return None;
        }
        // Metronome is fed in [`Self::report`] after the verdict. A damage-idle
        // hole must not advance the display-hardware beat.
        Some(Stall { gap })
    }

    /// Feed a verdicted stall into the metronome. `Some(mean period)` on a
    /// completed cycle. Damage-idle is not fed: an input pause on the user's
    /// cadence must not fabricate a display-disturbance beat.
    pub(crate) fn cycle(&mut self, now: Instant, damage_idle: bool) -> Option<Duration> {
        if damage_idle {
            return None;
        }
        self.cadence.note(now)
    }
}

/// Structured half of `EtwWatch::window_report`: compose-silence discriminator evidence.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EtwWindowCounts {
    /// Swapchain presents inside the window; the game and dwm both count.
    pub(crate) presents: u32,
    /// `BltQueueAddEntry` events inside the window (frames entering the kernel queue).
    pub(crate) queue_adds: u32,
    /// Present stream demonstrated liveness inside `LOOKBACK` before the hole — a
    /// working witness whose in-window zero is a reading, not a dead one whose zero is noise.
    pub(crate) present_history: bool,
    /// Queue-stream liveness inside `LOOKBACK` before the hole (`BltQueueAddEntry` or
    /// `BltQueueCompleteIndirectPresent` — either proves the witness works).
    pub(crate) queue_history: bool,
    /// Every lookback present came from `dwm.exe` (and there was at least one): pre-hole
    /// flow was desktop composition, not a game. Set by `EtwWatch::window_report` (name
    /// resolution lives there); [`attribute`] requires it so a game's holes are never demoted.
    pub(crate) flow_dwm_only: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed [`StallWatch`] at `offsets_ms`; metronome is non-damage-idle, as `report` feeds it.
    fn watch_run(offsets_ms: &[u64]) -> Vec<Option<(Stall, Option<Duration>)>> {
        let base = Instant::now();
        let mut w = StallWatch::new();
        offsets_ms
            .iter()
            .map(|ms| {
                let at = base + Duration::from_millis(*ms);
                w.note_fresh(at, None).map(|s| {
                    let period = w.cycle(at, false);
                    (s, period)
                })
            })
            .collect()
    }

    fn flow(out: &mut Vec<u64>, start_ms: u64, frames: u64) {
        out.extend((0..frames).map(|i| start_ms + i * 16));
    }

    #[test]
    fn stall_detected_after_active_flow() {
        // 20 frames of 60 fps, then a 300 ms hole — the resuming frame is a stall.
        let mut t = Vec::new();
        flow(&mut t, 0, 20); // last frame at 304 ms
        t.push(604);
        let out = watch_run(&t);
        assert!(out[..20].iter().all(Option::is_none));
        let (stall, period) = out[20].as_ref().expect("hole after active flow is a stall");
        assert_eq!(stall.gap.as_millis(), 300);
        assert!(period.is_none(), "one stall is not a cycle");
    }

    #[test]
    fn idle_desktop_gaps_are_not_stalls() {
        // ~530 ms caret blink: activity gate never opens.
        let t: Vec<u64> = (0..12).map(|i| i * 530).chain([20_000]).collect();
        assert!(watch_run(&t).iter().all(Option::is_none));
    }

    #[test]
    fn thirty_fps_content_still_qualifies_as_active() {
        // 33 ms cadence: 8 pre-gap frames span 231 ms ≤ ACTIVE_SPAN.
        let mut t: Vec<u64> = (0..10).map(|i| i * 33).collect(); // last at 297 ms
        t.push(497);
        let out = watch_run(&t);
        assert!(out[10].is_some(), "30 fps flow must pass the activity gate");
    }

    /// First degraded-stretch summary, checked after every frame like the capture loop.
    /// Every frame reports the same 40 ms present→arrival, so the folded tally is
    /// assertable without modelling which frames land inside the stretch.
    fn watch_recovery(offsets_ms: &[u64]) -> (StallWatch, Option<Recovery>) {
        let base = Instant::now();
        let mut w = StallWatch::new();
        let mut recovery = None;
        for ms in offsets_ms {
            w.note_fresh(base + Duration::from_millis(*ms), Some(40));
            if let Some(r) = w.take_recovery() {
                recovery.get_or_insert(r);
            }
        }
        (w, recovery)
    }

    #[test]
    fn a_degraded_stretch_summarizes_on_recovery() {
        // ~2 fps phase (10×500 ms holes) after active flow: one summary for the stretch.
        let mut t = Vec::new();
        flow(&mut t, 0, 20); // last frame at 304 ms
        t.extend((1..=10).map(|i| 304 + i * 500)); // 804..5304: ten 500 ms holes
        t.extend((1..=12).map(|i| 5304 + i * 16)); // sustained flow is back
        let (_, r) = watch_recovery(&t);
        let r = r.expect("a multi-hole degraded stretch summarizes at recovery");
        assert_eq!(r.holes, 10);
        assert_eq!(r.hole_time.as_millis(), 5000);
        assert_eq!(r.worst.as_millis(), 500);
        assert_eq!(r.degraded.as_millis(), 5000);
        // Every stamped frame reported 40 ms, at least one per hole.
        assert_eq!(r.arrival_ms().as_deref(), Some("40/40/40"));
        assert!(r.arrival_n >= r.holes, "n={}", r.arrival_n);
    }

    #[test]
    fn a_single_stall_never_summarizes() {
        // One hole in healthy flow: its stall line covers it; a one-hole stretch must not summarize.
        let mut t = Vec::new();
        flow(&mut t, 0, 20);
        t.push(604); // the lone 300 ms hole
        t.extend((1..=12).map(|i| 604 + i * 16));
        let (_, r) = watch_recovery(&t);
        assert!(
            r.is_none(),
            "single stall must not produce a stretch summary"
        );
    }

    #[test]
    fn a_reset_cut_stretch_still_summarizes() {
        // A reset clears flow history mid-stretch; holes before it must still surface.
        let mut t = Vec::new();
        flow(&mut t, 0, 20);
        t.extend((1..=3).map(|i| 304 + i * 500));
        let (mut w, r) = watch_recovery(&t);
        assert!(r.is_none(), "stretch still open — no summary yet");
        w.reset();
        let r = w
            .take_recovery()
            .expect("reset closes and summarizes the open stretch");
        assert_eq!(r.holes, 3);
        assert_eq!(r.hole_time.as_millis(), 1500);
    }

    #[test]
    fn a_content_stop_closes_the_stretch_without_folding_the_pause_in() {
        // Two degraded holes, then a 20 s pause. Summary covers the stretch only.
        let mut t = Vec::new();
        flow(&mut t, 0, 20);
        t.extend([804, 1304, 21_304]);
        let (_, r) = watch_recovery(&t);
        let r = r.expect("the content stop closes the stretch");
        assert_eq!(r.holes, 2);
        assert_eq!(r.hole_time.as_millis(), 1000);
        assert_eq!(r.degraded.as_millis(), 1000);
    }

    #[test]
    fn metronomic_stalls_self_diagnose() {
        // ~300 ms DWM holes every 4 s in 60 fps flow. 5 cycles → 4 stalls; the 4th is the period.
        let mut t = Vec::new();
        for cycle in 0..5u64 {
            // ~3.7 s of flow, then the hole to the next cycle.
            flow(&mut t, cycle * 4_000, 232); // last frame at cycle*4000 + 3696
        }
        let out = watch_run(&t);
        let stalls: Vec<&(Stall, Option<Duration>)> = out.iter().flatten().collect();
        assert_eq!(stalls.len(), 4, "each cycle boundary is one stall");
        assert!(stalls[..3].iter().all(|(_, period)| period.is_none()));
        let period = stalls[3]
            .1
            .expect("the 4th evenly-spaced event completes the metronome streak");
        assert!(
            (period.as_secs_f64() - 4.0).abs() < 0.3,
            "period={period:?}"
        );
    }

    /// Same four evenly-spaced stalls as [`metronomic_stalls_self_diagnose`], one
    /// damage-idle: a hand/input pause is not display-disturbance evidence.
    #[test]
    fn damage_idle_stalls_do_not_feed_the_metronome() {
        let base = Instant::now();
        let mut w = StallWatch::new();
        let mut periods = Vec::new();
        for cycle in 0..5u64 {
            let mut t = Vec::new();
            flow(&mut t, cycle * 4_000, 232);
            for ms in t {
                let at = base + Duration::from_millis(ms);
                if let Some(_stall) = w.note_fresh(at, None) {
                    // 2nd stall is damage-idle (cursor still on a dwm-only desktop).
                    let damage_idle = periods.len() == 1;
                    periods.push(w.cycle(at, damage_idle));
                }
            }
        }
        assert_eq!(periods.len(), 4);
        assert!(
            periods.iter().all(Option::is_none),
            "a skipped beat must break the streak: {periods:?}"
        );
    }

    #[test]
    fn reset_swallows_the_restart_gap() {
        // Restart, then resume 800 ms later: not a stall; detection re-arms after.
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        let mut w = StallWatch::new();
        for i in 0..20u64 {
            assert!(w.note_fresh(at(i * 16), None).is_none());
        }
        w.reset();
        assert!(
            w.note_fresh(at(1_104), None).is_none(),
            "restart gap swallowed"
        );
        for i in 1..20u64 {
            assert!(w.note_fresh(at(1_104 + i * 16), None).is_none());
        }
        assert!(
            w.note_fresh(at(1_104 + 19 * 16 + 300), None).is_some(),
            "detection re-armed after the reset"
        );
    }

    /// Third stall in 60 s warns; quiet through 300 s re-warn spacing; re-arms after age-out.
    #[test]
    fn stall_rate_warn_window_and_rewarn() {
        let base = Instant::now();
        let at = |s: u64| base + Duration::from_secs(s);
        let mut w = StallWatch::new();
        assert_eq!(w.note_for_rate_warn(at(0)), None);
        assert_eq!(w.note_for_rate_warn(at(10)), None);
        assert_eq!(
            w.note_for_rate_warn(at(20)),
            Some(3),
            "third stall in 60 s warns"
        );
        assert_eq!(
            w.note_for_rate_warn(at(30)),
            None,
            "inside the re-warn spacing the arm stays quiet"
        );
        // Past the spacing: old entries aged out, so RATE_MIN_STALLS again then re-warns.
        assert_eq!(w.note_for_rate_warn(at(400)), None);
        assert_eq!(w.note_for_rate_warn(at(401)), None);
        assert_eq!(
            w.note_for_rate_warn(at(402)),
            Some(3),
            "re-warns after the spacing"
        );
    }

    /// [`attribute`] verdict table: the drain heartbeat, then the cursor witness.
    #[test]
    fn stall_attribution_verdicts() {
        let verdict = |gap_ms: u64, hb_age_ms: Option<u64>, moved: Option<u32>| {
            attribute(
                Duration::from_millis(gap_ms),
                &StallEvidence {
                    max_heartbeat_age_ms: hb_age_ms,
                    probes: None,
                    etw: None,
                    etw_counts: None,
                    cursor_moved_px: moved,
                },
            )
        };
        // No encoder open yet: no heartbeat, no verdict.
        assert_eq!(verdict(300, None, None), StallVerdict::NoTelemetry);
        // Heartbeat silent for most of the hole → worker starved.
        assert_eq!(verdict(600, Some(400), None), StallVerdict::WorkerStalled);
        // ≤16 ms heartbeat; 200 ms silence on a 300 ms gap is under max(gap/2, 250 ms).
        assert_eq!(verdict(300, Some(200), None), StallVerdict::ComposeSilence);
        assert_eq!(
            verdict(300, Some(20), Some(312)),
            StallVerdict::ComposeSilence
        );
        // Long holes scale the bar: 900 ms silence on a 3 s gap is not half.
        assert_eq!(
            verdict(3_000, Some(900), None),
            StallVerdict::ComposeSilence
        );
        assert_eq!(
            verdict(3_000, Some(1_600), None),
            StallVerdict::WorkerStalled
        );
        // The cursor never moved through the hole: nothing was dirty.
        assert_eq!(verdict(600, Some(16), Some(0)), StallVerdict::DamageIdle);
        // A starved worker is never demoted by a still cursor.
        assert_eq!(
            verdict(600, Some(400), Some(0)),
            StallVerdict::WorkerStalled
        );
    }

    /// With the ETW leg on (`PUNKTFUNK_IDD_DIAG`), a game presenting through the hole keeps
    /// compose-silence even under a still cursor; dwm-only flow still demotes.
    #[test]
    fn a_present_witness_blocks_the_damage_idle_demotion() {
        let verdict = |dwm_only: bool| {
            attribute(
                Duration::from_millis(600),
                &StallEvidence {
                    max_heartbeat_age_ms: Some(16),
                    probes: None,
                    etw: None,
                    etw_counts: Some(EtwWindowCounts {
                        presents: 40,
                        queue_adds: 0,
                        present_history: true,
                        queue_history: true,
                        flow_dwm_only: dwm_only,
                    }),
                    cursor_moved_px: Some(0),
                },
            )
        };
        assert_eq!(verdict(false), StallVerdict::ComposeSilence);
        assert_eq!(verdict(true), StallVerdict::DamageIdle);
    }
}
