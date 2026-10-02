//! Capture-stall detection for IDD-push: holes in DWM frame delivery while
//! the desktop was composing.
//!
//! [`StallWatch`] gates on recent active flow, names each hole from the two
//! clocks the driver stamps — the drain heartbeat, and the pool taking a frame
//! — and feeds a metronome so periodic stalls self-diagnose. Damage-idle holes
//! (the cursor never moved) are real delivery gaps but stay out of the beat.
//!
//! Probes and the DxgKrnl ETW session ride the report as evidence when
//! `PUNKTFUNK_IDD_DIAG` turned them on; they name no class.

use super::*;

/// A hole in DWM delivery that opened after recent active compose ([`StallWatch`]).
///
/// The metronome is not fed here. [`StallWatch::report`] feeds it after the
/// verdict, so a damage-idle hole never advances the display-hardware beat.
pub(super) struct Stall {
    pub(super) gap: Duration,
}

/// One degraded stretch, closed by [`StallWatch::take_recovery`].
///
/// Per-hole stall lines gate on prior active flow, so a sustained slow phase
/// logs only the first hole; this summary is the stretch's remaining line.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Recovery {
    pub(super) degraded: Duration,
    /// Stall-sized holes (≥ [`StallWatch::STALL_MIN`]).
    pub(super) holes: u32,
    pub(super) hole_time: Duration,
    pub(super) worst: Duration,
    /// Present→arrival over the stretch's frames (ms): the access unit's OS
    /// present stamp against the moment the host took it. `arrival_n` counts
    /// the frames that carried a stamp; 0 means none did, not a zero delay.
    pub(super) arrival_last_ms: u64,
    pub(super) arrival_mean_ms: u64,
    pub(super) arrival_max_ms: u64,
    pub(super) arrival_n: u32,
}

impl Recovery {
    /// `last/mean/max` present→arrival ms for the log line; `None` when no frame
    /// in the stretch carried an OS present stamp.
    pub(super) fn arrival_ms(&self) -> Option<String> {
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
pub(super) struct StallEvidence {
    /// Max `now − drain heartbeat` over the window, in milliseconds. `None` before the
    /// encoder is open, when the driver reports nothing.
    pub(super) max_heartbeat_age_ms: Option<u64>,
    pub(super) probes: Option<ProbeWindow>,
    /// DxgKrnl DDI summary for the window. `None` when the ETW session is unavailable.
    pub(super) etw: Option<String>,
    /// Present-vs-queue counts ([`EtwWatch::window_report`]). Presents flowing
    /// while the queue starves = OS dropped composed frames; both silent =
    /// content stopped. `None` when the ETW session is unavailable.
    pub(super) etw_counts: Option<super::dxgkrnl_etw::EtwWindowCounts>,
    /// Cursor travel during the hole (px, |dx|+|dy|). `Some(0)` = nothing to
    /// compose (damage-idle), also under a declared hardware cursor, whose
    /// travel composes nothing; `Some(n>0)` = damage existed and DWM composed
    /// none of it. `None` = never sampled. The stall-ending frame's own move
    /// is not counted (capturer fold-on-next-call sampler).
    pub(super) cursor_moved_px: Option<u32>,
}

/// Per-leg maxima from `probes::ProbeEngine::window` across one stall.
/// Every field is `None` when that probe is absent; absence is never guessed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ProbeWindow {
    pub(super) fence_max_us: Option<u64>,
    /// Longest span with no `DwmGetCompositionTimingInfo` `cRefresh` advance (µs).
    pub(super) dwm_tick_frozen_us: Option<u64>,
    /// Longest span with no `cFrame` advance (µs). Advisory only: on Win11
    /// `DWM_TIMING_INFO.cFrame` is refresh-synthesized and ticks without composes.
    pub(super) dwm_frame_frozen_us: Option<u64>,
    pub(super) dwm_flush_max_us: Option<u64>,
    /// Worst `D3DKMTGetScanLine` call latency (µs). Blocking here convicts the KMD.
    pub(super) scanline_max_us: Option<u64>,
    /// Physical head present. Exclusive topology leaves only our IDD; latency
    /// still counts, scanline values do not.
    pub(super) scanline_physical: bool,
    /// Worst high-res sleeper overshoot (µs). DPC-storm / CPU-starvation discriminator.
    pub(super) cpu_max_overshoot_us: Option<u64>,
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

/// Driver GPU-priority lever (`PFVD_NO_RT_GPU` opt-out; default REALTIME).
/// Reads this process's env; WUDFHost resolves the same variable. An env
/// edited after either process started is stale until restart. Rides every
/// stall-triage line so an A/B is interpretable; lowering priority masks
/// this class at most.
pub(super) fn rt_gpu_driver_posture() -> &'static str {
    if std::env::var_os("PFVD_NO_RT_GPU").is_some() {
        "off (PFVD_NO_RT_GPU)"
    } else {
        "REALTIME (default)"
    }
}

pub(super) fn rt_gpu_host_posture() -> &'static str {
    match std::env::var("PUNKTFUNK_GPU_PRIORITY_CLASS")
        .ok()
        .as_deref()
    {
        Some("off") => "off",
        Some("normal") => "normal",
        Some("high") => "high",
        _ => "REALTIME (default)",
    }
}

/// What one hole is, from the driver's own clocks. Probes and ETW ride the
/// report as evidence; they no longer name a class of their own.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum StallVerdict {
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
pub(super) fn attribute(gap: Duration, evidence: &StallEvidence) -> StallVerdict {
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
/// recovery-cadence detector. The caller logs.
pub(super) struct StallWatch {
    /// Last [`Self::RECENT`] fresh-frame instants — activity-gate history.
    recent: std::collections::VecDeque<Instant>,
    cadence: pf_frame::metronome::Metronome,
    /// Session stall count and how many carried a coinciding OS display event.
    seen: u32,
    with_os_events: u32,
    /// Per-verdict counts in [`StallVerdict`] order. The metronomic WARN prints
    /// the session, not just the stall that tripped the beat.
    verdicts: [u32; 4],
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

    pub(super) fn new() -> Self {
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
    pub(super) fn note_for_rate_warn(&mut self, now: Instant) -> Option<usize> {
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
    fn verdict_tally(&self) -> String {
        format!(
            "worker-stalled {}, compose-silence {}, damage-idle {}, no-telemetry {}",
            self.verdicts[1], self.verdicts[2], self.verdicts[3], self.verdicts[0]
        )
    }

    /// Drop flow history. A presentation-restart gap is self-inflicted; without this the
    /// first frame after it reads as a stall. Open episodes still close — those holes
    /// predate the restart.
    pub(super) fn reset(&mut self) {
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
    pub(super) fn take_recovery(&mut self) -> Option<Recovery> {
        self.pending_recovery.take()
    }

    /// Record a fresh driver frame at `now`, with its present→arrival in ms if the access unit
    /// carried an OS present stamp. `Some` iff the frame ended a stall.
    pub(super) fn note_fresh(&mut self, now: Instant, arrival_ms: Option<u64>) -> Option<Stall> {
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
    pub(super) fn cycle(&mut self, now: Instant, damage_idle: bool) -> Option<Duration> {
        if damage_idle {
            return None;
        }
        self.cadence.note(now)
    }

    /// Log a stall, correlate OS display events, and name the cause once the
    /// cadence is metronomic.
    ///
    /// `now` is the frame that ended the stall (same instant as [`Self::note_fresh`])
    /// and bounds the event-correlation window. `evidence` is the capturer's
    /// sample for that window; [`attribute`] rides every stall line.
    pub(super) fn report(&mut self, stall: &Stall, now: Instant, evidence: &StallEvidence) {
        // Gap plus 300 ms lead-in: the causing OS event lands just before
        // DWM stops delivering.
        let window = stall.gap + Duration::from_millis(300);
        let events = now
            .checked_sub(window)
            .map(|from| pf_win_display::display_events::events_between(from, now))
            .unwrap_or_default();
        self.seen = self.seen.saturating_add(1);
        if !events.is_empty() {
            self.with_os_events = self.with_os_events.saturating_add(1);
        }
        let verdict = attribute(stall.gap, evidence);
        self.verdicts[match verdict {
            StallVerdict::NoTelemetry => 0,
            StallVerdict::WorkerStalled => 1,
            StallVerdict::ComposeSilence => 2,
            StallVerdict::DamageIdle => 3,
        }] += 1;
        // Damage-idle is still a delivery hole (episode/recovery count it) but
        // not display-disturbance evidence: skip metronome, rate WARN, and
        // connected-inactive trial. The per-stall line still carries evidence.
        let damage_idle = verdict == StallVerdict::DamageIdle;
        let metronomic = self.cycle(now, damage_idle);
        // debug, not warn: a single hole is a legitimate content pause. The
        // reportable signal is the metronomic cycle below.
        tracing::debug!(
            gap_ms = stall.gap.as_millis() as u64,
            os_display_events = %pf_win_display::display_events::summarize(&events),
            verdict = %verdict,
            probes = evidence.probes.as_ref().map(tracing::field::display),
            etw = evidence.etw.as_deref().unwrap_or("unavailable"),
            // Presents from any process vs virtual-display kernel-queue adds,
            // both only under PUNKTFUNK_IDD_DIAG.
            etw_presents = evidence.etw_counts.map(|c| c.presents),
            etw_queue_adds = evidence.etw_counts.map(|c| c.queue_adds),
            // 0 = nothing to compose. >0 = damage existed and DWM composed none of it.
            cursor_moved_px_during_gap = evidence.cursor_moved_px,
            flow_dwm_only = evidence.etw_counts.map(|c| c.flow_dwm_only),
            max_heartbeat_age_ms = evidence.max_heartbeat_age_ms,
            "IDD-push capture stall — the desktop was composing at speed, then the driver's \
             pool took no frame for the gap; the verdict names the clock that stopped"
        );
        // Aperiodic 150+ ms holes still need the triage payload. Skip when
        // this stall completed a metronomic cycle (richer arms below) or is
        // damage-idle.
        if metronomic.is_none() && !damage_idle {
            if let Some(stalls_in_window) = self.note_for_rate_warn(now) {
                let suspects = pf_win_display::display_events::connected_inactive_physicals();
                let suspects = if suspects.is_empty() {
                    "none".to_string()
                } else {
                    suspects.join(", ")
                };
                tracing::warn!(
                    stalls_in_window = stalls_in_window as u64,
                    os_correlated = format!("{}/{}", self.with_os_events, self.seen),
                    connected_inactive = %suspects,
                    rt_gpu_driver = rt_gpu_driver_posture(),
                    rt_gpu_host = rt_gpu_host_posture(),
                    verdicts = %self.verdict_tally(),
                    "capture stalls are REPEATING without a stable period — same triage as the \
                     metronomic arm: a connected-but-inactive display's standby servicing \
                     (see connected_inactive), then display-poller software (the SteelSeries \
                     GG / SignalRGB class). Lowering the GPU-priority defaults \
                     (setx /M PFVD_NO_RT_GPU 1 / PUNKTFUNK_GPU_PRIORITY_CLASS=high) has \
                     quieted some AMD boxes but masks this class at most — attenuation, not \
                     attribution. A compose-silence tally does NOT exonerate the display \
                     stack — a frozen presenter reads identically"
                );
            }
        }
        if let Some(period) = metronomic {
            let suspects = pf_win_display::display_events::connected_inactive_physicals();
            let suspects = if suspects.is_empty() {
                "none".to_string()
            } else {
                suspects.join(", ")
            };
            let correlated = format!("{}/{}", self.with_os_events, self.seen);
            let verdict_tally = self.verdict_tally();
            // ≥ half the stalls with a coinciding OS event: the cascade is
            // OS-visible. Otherwise it never surfaces above the driver.
            if self.with_os_events * 2 >= self.seen {
                tracing::warn!(
                    period_s = format!("{:.2}", period.as_secs_f64()),
                    os_correlated = correlated,
                    connected_inactive = %suspects,
                    verdicts = %verdict_tally,
                    "capture stalls are METRONOMIC and coincide with Windows monitor \
                     hot-plug/re-enumeration events — a connected display (or its \
                     cable/switch/AVR) re-probes the link on a timer and Windows re-reacts \
                     each time. Cures, best-first: that display's OSD 'auto input \
                     scan/detect' OFF (and on TVs: instant-on/quick-start + CEC off), \
                     unplug its cable at the GPU, an HPD-holding adapter/dummy plug, or \
                     keep it active while streaming; the pnp_disable_monitors policy axis \
                     suppresses the Windows-side reaction (see connected_inactive for the \
                     suspects)"
                );
            } else {
                // Both REALTIME GPU-priority levers (default on). The line
                // must say whether either is engaged so an A/B is interpretable.
                let rt_gpu_driver = rt_gpu_driver_posture();
                let rt_gpu_host = rt_gpu_host_posture();
                tracing::warn!(
                    period_s = format!("{:.2}", period.as_secs_f64()),
                    os_correlated = correlated,
                    connected_inactive = %suspects,
                    rt_gpu_driver,
                    rt_gpu_host,
                    verdicts = %verdict_tally,
                    "capture stalls are METRONOMIC with NO coinciding OS display event — \
                     the disturbance is BELOW Windows (damage-idle holes — the cursor \
                     stationary through the hole, i.e. input pauses — are already \
                     excluded from this beat; see cursor_moved_px_during_gap on the \
                     per-stall lines). Suspects: the GPU driver servicing a \
                     connected-but-asleep sink (standby HPD/DDC/link probing), \
                     display-poller software (the SteelSeries-GG/SignalRGB class — \
                     correlate 'slow display-descriptor poll' lines), or the DWM present \
                     clock (try a different refresh rate). Lowering the GPU-priority \
                     defaults (setx /M PFVD_NO_RT_GPU 1 / \
                     PUNKTFUNK_GPU_PRIORITY_CLASS=high) has quieted some AMD boxes but \
                     masks this class at most — a quiet A/B is attenuation, not \
                     attribution. If connected_inactive lists a \
                     display, its standby servicing is a suspect — cursor motion through \
                     the holes is what convicts the display stack. For an external \
                     display: keep it active while streaming, disable its OSD auto input \
                     scan (TVs: instant-on/quick-start + CEC off), unplug it at the GPU, \
                     or use an HPD-holding adapter/dummy. For a LAPTOP PANEL: keep it \
                     active with `topology: primary` (the dark-but-connected-head \
                     hypothesis has no confirmed post-0.28 case — verify with the cursor \
                     witness before chasing it)"
                );
            }
        }
    }
}
