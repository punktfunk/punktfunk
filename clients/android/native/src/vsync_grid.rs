//! The vsync clock's arithmetic: which frame timeline a release targets, how it lands on the
//! panel's grid and how the callback period is smoothed. Plain values in and out; the
//! `AChoreographer` thread and the atomics it publishes to are in `decode/vsync.rs`.

/// One upcoming frame timeline (API 33+ payload): when SurfaceFlinger expects to present the
/// frame, and the last instant it can be submitted to make that present. Monotonic ns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameTimeline {
    pub(crate) expected_present_ns: i64,
    pub(crate) deadline_ns: i64,
}

/// The timeline for a frame that must not be presented before `not_before_ns`: the earliest
/// stored one whose EXPECTED PRESENT is past that instant, else the last one stepped forward by
/// whole `period_ns` once the stored set has aged out (timelines refresh once per vsync callback;
/// a frame can decode anywhere inside that window). `None` for an empty set (the 31/32 fallback:
/// the caller releases ASAP) and for an aged-out one with no period measured.
///
/// The floor is the caller's to compose: SurfaceFlinger's latch lead (`now + margin`) and, under
/// cadence targeting, the frame's own due time on the source's timeline. Adding the submit lead
/// here to a due time that is already an absolute present instant would push a frame due just
/// under a grid point onto the next one for some phases and not others: judder, not latency.
///
/// The gate is `expected_present`, not the timeline's `deadline`. The deadline budgets for GPU
/// rendering the app has yet to submit, which can exceed a full 120 Hz period, but a video buffer
/// is already rendered: the one constraint is SurfaceFlinger's latch lead. A mis-gamble presents
/// one vsync later.
pub(crate) fn pick_timeline(
    timelines: &[FrameTimeline],
    period_ns: i64,
    not_before_ns: i64,
) -> Option<FrameTimeline> {
    if let Some(t) = timelines
        .iter()
        .find(|t| t.expected_present_ns > not_before_ns)
    {
        return Some(*t);
    }
    let last = *timelines.last()?;
    if period_ns <= 0 {
        return None;
    }
    let behind = not_before_ns.saturating_sub(last.expected_present_ns);
    let k = behind / period_ns + 1;
    Some(FrameTimeline {
        expected_present_ns: last.expected_present_ns + k * period_ns,
        deadline_ns: last.deadline_ns + k * period_ns,
    })
}

/// `t` pulled earlier by whole panel periods while its present still clears `not_before_ns`.
/// The platform reports timelines at the app's assigned render rate, but the panel latches at its
/// own: an app down-rated to 60 Hz callbacks on a 120 Hz panel sees timelines a whole panel period
/// apart, and pacing to them would cap the video. At the panel's own rate the pull never fires,
/// and an unknown panel period (0) leaves `t` as it is.
pub(crate) fn onto_panel_grid(
    mut t: FrameTimeline,
    panel_ns: i64,
    not_before_ns: i64,
) -> FrameTimeline {
    if panel_ns > 0 {
        while t.expected_present_ns - panel_ns > not_before_ns {
            t.deadline_ns -= panel_ns;
            t.expected_present_ns -= panel_ns;
        }
    }
    t
}

/// The vsync period after one callback, or `None` to keep `old_ns`. The observed period is the
/// spacing of the first two timelines (exact, straight from the platform), else the delta of
/// successive callbacks (jittery), and counts only within 24..500 Hz. An observation moves the
/// estimate an eighth of the way; the first one is taken whole. `prev_frame_ns` is 0 before a
/// callback has run.
pub(crate) fn smoothed_period(
    old_ns: i64,
    timelines: &[FrameTimeline],
    frame_time_ns: i64,
    prev_frame_ns: i64,
) -> Option<i64> {
    let mut period = 0i64;
    if timelines.len() >= 2 {
        period = timelines[1].expected_present_ns - timelines[0].expected_present_ns;
    } else if prev_frame_ns > 0 {
        period = frame_time_ns - prev_frame_ns;
    }
    (2_000_000..=42_000_000).contains(&period).then(|| {
        if old_ns > 0 {
            (old_ns * 7 + period) / 8
        } else {
            period
        }
    })
}
