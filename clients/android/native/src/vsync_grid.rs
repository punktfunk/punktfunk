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
/// a frame can decode anywhere inside that window). `None` for an empty set (below API 33, the
/// `postFrameCallback64` fallback in `decode/vsync.rs`: the caller releases ASAP) and for an
/// aged-out one with no period measured.
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

/// The first vsync past `after_ns` on the callback grid: `last_vsync_ns` stepped by whole
/// `period_ns`. `None` before the first callback (0) or a measured period.
pub(crate) fn next_vsync(last_vsync_ns: i64, period_ns: i64, after_ns: i64) -> Option<i64> {
    if last_vsync_ns <= 0 || period_ns <= 0 {
        return None;
    }
    let k = after_ns.saturating_sub(last_vsync_ns).div_euclid(period_ns) + 1;
    Some(last_vsync_ns + k * period_ns)
}

/// What a display states about its refresh, read where a vsync callback carries no frame
/// timeline (below API 33): `Display.getAppVsyncOffsetNanos` and
/// `Display.getPresentationDeadlineNanos`. Zero is unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GridFacts {
    /// How far a callback's frame time trails the refresh it reports.
    pub(crate) app_offset_ns: i64,
    /// How long before a refresh SurfaceFlinger must hold the buffer it shows there.
    pub(crate) lead_ns: i64,
}

impl GridFacts {
    /// The callback's offset where it is inside one `step_ns`, else 0.
    pub(crate) fn offset_on(self, step_ns: i64) -> i64 {
        if (0..step_ns).contains(&self.app_offset_ns) {
            self.app_offset_ns
        } else {
            0
        }
    }

    /// The latch lead where it is within two of `period_ns`, else one period: a compositor
    /// that wakes once a refresh makes a present a whole period ahead.
    pub(crate) fn lead_on(self, period_ns: i64) -> i64 {
        if self.lead_ns > 0 && self.lead_ns <= 2 * period_ns {
            self.lead_ns
        } else {
            period_ns.max(0)
        }
    }
}

/// Where a release is aimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Aim {
    /// The refresh the frame is for.
    pub(crate) present_ns: i64,
    /// The desired present time the buffer carries. SurfaceFlinger shows it at the first
    /// refresh at or after this, and of two due for one refresh it shows the later.
    pub(crate) stamp_ns: i64,
}

impl Aim {
    /// A platform timeline names its refresh exactly.
    pub(crate) fn at(t: FrameTimeline) -> Aim {
        Aim {
            present_ns: t.expected_present_ns,
            stamp_ns: t.expected_present_ns,
        }
    }
}

/// The aim on the callback grid for a frame that must not be presented before `not_before_ns`:
/// the first refresh past it, counted from `last_vsync_ns − app_offset_ns` in whole `step_ns`.
/// The stamp sits half a step before that refresh, so an offset or a period that is off by less
/// still names the same one. `None` before the first callback (0) or a measured step.
pub(crate) fn grid_aim(
    last_vsync_ns: i64,
    step_ns: i64,
    app_offset_ns: i64,
    not_before_ns: i64,
) -> Option<Aim> {
    let present_ns = next_vsync(last_vsync_ns - app_offset_ns, step_ns, not_before_ns)?;
    Some(Aim {
        present_ns,
        stamp_ns: present_ns - step_ns / 2,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tl(expected_present_ns: i64, deadline_ns: i64) -> FrameTimeline {
        FrameTimeline {
            expected_present_ns,
            deadline_ns,
        }
    }

    /// Three timelines a 16 ns app period apart, the deadline 10 before each present.
    fn set() -> [FrameTimeline; 3] {
        [tl(100, 90), tl(116, 106), tl(132, 122)]
    }

    #[test]
    fn the_pick_is_the_earliest_timeline_past_the_floor() {
        // (name, floor, period, pick)
        let rows = [
            (
                "the first when the floor is before it",
                99,
                16,
                Some(tl(100, 90)),
            ),
            (
                "a present on the floor does not count",
                100,
                16,
                Some(tl(116, 106)),
            ),
            ("between two", 110, 16, Some(tl(116, 106))),
            (
                "the last, one before its present",
                131,
                16,
                Some(tl(132, 122)),
            ),
            (
                "the period is not read while a timeline clears",
                99,
                0,
                Some(tl(100, 90)),
            ),
            (
                "aged out on the last present steps one period",
                132,
                16,
                Some(tl(148, 138)),
            ),
            ("aged out inside a period", 140, 16, Some(tl(148, 138))),
            (
                "aged out on a stepped present steps again",
                148,
                16,
                Some(tl(164, 154)),
            ),
            ("aged out by many periods", 1000, 16, Some(tl(1012, 1002))),
            ("aged out with no period measured", 140, 0, None),
            ("aged out with a negative period", 140, -16, None),
        ];
        for (name, floor, period, want) in rows {
            assert_eq!(pick_timeline(&set(), period, floor), want, "{name}");
        }
    }

    #[test]
    fn an_empty_set_has_no_pick() {
        assert_eq!(pick_timeline(&[], 16, 100), None, "the 31/32 fallback");
        assert_eq!(pick_timeline(&[], 0, 100), None);
    }

    #[test]
    fn a_pick_is_pulled_onto_the_panel_grid() {
        // (name, timeline, panel period, floor, result)
        let rows = [
            (
                "one panel period earlier",
                tl(116, 106),
                8,
                100,
                tl(108, 98),
            ),
            (
                "down to the last present clear of the floor",
                tl(116, 106),
                8,
                80,
                tl(84, 74),
            ),
            (
                "callbacks at the panel rate stay",
                tl(116, 106),
                16,
                100,
                tl(116, 106),
            ),
            (
                "a panel period longer than the app's stays",
                tl(116, 106),
                32,
                100,
                tl(116, 106),
            ),
            (
                "a present just past the floor stays",
                tl(101, 91),
                8,
                100,
                tl(101, 91),
            ),
            (
                "an unknown panel period stays",
                tl(116, 106),
                0,
                100,
                tl(116, 106),
            ),
            (
                "a negative panel period stays",
                tl(116, 106),
                -8,
                100,
                tl(116, 106),
            ),
        ];
        for (name, t, panel, floor, want) in rows {
            assert_eq!(onto_panel_grid(t, panel, floor), want, "{name}");
        }
    }

    #[test]
    fn the_next_vsync_is_the_first_grid_point_past_the_instant() {
        // (name, last vsync, period, after, result)
        let rows = [
            ("inside the period", 100, 16, 105, Some(116)),
            ("on a vsync, the one after", 100, 16, 100, Some(116)),
            ("one before the next", 100, 16, 115, Some(116)),
            ("callbacks skipped", 100, 16, 150, Some(164)),
            ("a callback stamped ahead of now", 100, 16, 99, Some(100)),
            ("no callback yet", 0, 16, 105, None),
            ("no period measured", 100, 0, 105, None),
        ];
        for (name, last, period, after, want) in rows {
            assert_eq!(next_vsync(last, period, after), want, "{name}");
        }
    }

    #[test]
    fn a_grid_aim_is_the_first_refresh_past_the_floor_stamped_half_a_step_early() {
        // Callbacks at 100, 116, …; the display refreshes 3 before each: 97, 113, 129, …
        // (name, last vsync, step, app offset, floor, (present, stamp))
        let rows = [
            ("the next refresh", 100, 16, 3, 100, Some((113, 105))),
            (
                "a refresh on the floor does not count",
                100,
                16,
                3,
                113,
                Some((129, 121)),
            ),
            ("one before a refresh", 100, 16, 3, 112, Some((113, 105))),
            (
                "a floor before the last refresh",
                100,
                16,
                3,
                90,
                Some((97, 89)),
            ),
            ("no stated offset", 100, 16, 0, 100, Some((116, 108))),
            ("callbacks skipped", 100, 16, 3, 150, Some((161, 153))),
            ("no callback yet", 0, 16, 3, 100, None),
            ("no step measured", 100, 0, 3, 100, None),
        ];
        for (name, last, step, offset, floor, want) in rows {
            let want = want.map(|(present_ns, stamp_ns)| Aim {
                present_ns,
                stamp_ns,
            });
            assert_eq!(grid_aim(last, step, offset, floor), want, "{name}");
        }
        // A platform timeline is its own stamp.
        assert_eq!(
            Aim::at(tl(116, 106)),
            Aim {
                present_ns: 116,
                stamp_ns: 116
            }
        );
    }

    #[test]
    fn stated_display_facts_count_only_where_they_are_plausible() {
        let facts = |app_offset_ns, lead_ns| GridFacts {
            app_offset_ns,
            lead_ns,
        };
        const P: i64 = 16_700_000;
        // (name, facts, (offset, lead))
        let rows = [
            (
                "as stated",
                facts(1_000_000, 11_700_000),
                (1_000_000, 11_700_000),
            ),
            ("a lead of two periods", facts(0, 2 * P), (0, 2 * P)),
            (
                "a lead past two periods is one",
                facts(0, 2 * P + 1),
                (0, P),
            ),
            ("no lead stated is one period", facts(0, 0), (0, P)),
            ("a negative lead is one period", facts(0, -1), (0, P)),
            ("an offset of a whole period is none", facts(P, P), (0, P)),
            ("a negative offset is none", facts(-1, P), (0, P)),
        ];
        for (name, f, (offset, lead)) in rows {
            assert_eq!((f.offset_on(P), f.lead_on(P)), (offset, lead), "{name}");
        }
        assert_eq!(facts(5, 5).lead_on(0), 0, "no period, no lead");
    }

    #[test]
    fn the_period_takes_an_eighth_of_each_observation() {
        const MS: i64 = 1_000_000;
        let spaced = |gap: i64| [tl(100 * MS, 90 * MS), tl(100 * MS + gap, 90 * MS + gap)];
        // (name, old, timelines, frame time, previous frame time, result)
        let rows = [
            (
                "the first spacing is taken whole",
                0,
                &spaced(8_333_333)[..],
                0,
                0,
                Some(8_333_333),
            ),
            (
                "a spacing moves the old by an eighth",
                8 * MS,
                &spaced(16 * MS)[..],
                0,
                0,
                Some(9 * MS),
            ),
            (
                "a spacing outranks the callback delta",
                8 * MS,
                &spaced(8 * MS)[..],
                1000 * MS,
                984 * MS,
                Some(8 * MS),
            ),
            (
                "the first callback delta is taken whole",
                0,
                &[][..],
                1016 * MS,
                1000 * MS,
                Some(16 * MS),
            ),
            (
                "a delta moves the old by an eighth",
                8 * MS,
                &[][..],
                1016 * MS,
                1000 * MS,
                Some(9 * MS),
            ),
            (
                "one timeline is no spacing",
                0,
                &[tl(100, 90)][..],
                1016 * MS,
                1000 * MS,
                Some(16 * MS),
            ),
            (
                "no timelines and no previous callback",
                8 * MS,
                &[][..],
                1000 * MS,
                0,
                None,
            ),
            (
                "500 Hz is the fastest",
                0,
                &spaced(2 * MS)[..],
                0,
                0,
                Some(2 * MS),
            ),
            (
                "faster than 500 Hz is dropped",
                8 * MS,
                &spaced(2 * MS - 1)[..],
                0,
                0,
                None,
            ),
            (
                "24 Hz is the slowest",
                0,
                &spaced(42 * MS)[..],
                0,
                0,
                Some(42 * MS),
            ),
            (
                "slower than 24 Hz is dropped",
                8 * MS,
                &spaced(42 * MS + 1)[..],
                0,
                0,
                None,
            ),
            (
                "a clock that went back is dropped",
                8 * MS,
                &[][..],
                1000 * MS,
                1016 * MS,
                None,
            ),
            (
                "an out-of-range spacing does not fall back to the delta",
                8 * MS,
                &spaced(0)[..],
                1016 * MS,
                1000 * MS,
                None,
            ),
            (
                "the average truncates",
                8_000_001,
                &spaced(8 * MS)[..],
                0,
                0,
                Some(8 * MS),
            ),
        ];
        for (name, old, timelines, frame, prev, want) in rows {
            assert_eq!(smoothed_period(old, timelines, frame, prev), want, "{name}");
        }
    }
}
