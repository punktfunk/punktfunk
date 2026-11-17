//! The presenter's verdicts: evidence, read once a second, that this device's present path does
//! not work the way the policy assumes. Pure; the sinks feed them and act.
//!
//!  * [`downrated`]: the panel runs at least 1.5× the period Kotlin asked for. The budgets stop
//!    holding frames back, short of [`super::budget::PRESENTED_CAP`].
//!  * [`Verdict::ShowsNothing`]: presents go out for two seconds and none is confirmed, on a sink
//!    whose confirms are live. The loop ends the run.
//!  * [`Verdict::Skipping`]: five unbroken seconds at or under the panel rate in which the store
//!    evicts a fifth of the decoded frames. The sink stops pacing for the session.
//!
//! A healthy device produces neither verdict. Design: `design/android-presenter-overhaul-v3.md`
//! §3.4 (planning repo).

/// Consecutive seconds without a confirm that make [`Verdict::ShowsNothing`].
const DARK_SECONDS: u32 = 2;

/// Consecutive skipping seconds that make [`Verdict::Skipping`].
const SKIPPING_SECONDS: u32 = 5;

/// The panel period is at least 1.5× `asked_period_ns`, the mode-table rate Kotlin asked for: the
/// system down-rated the panel below the stream. A real 60 Hz panel under a 120 fps stream asks
/// for 60 Hz, since its mode table has no faster mode, so it never reads as down-rated. Either
/// period 0 (unknown) turns the rule off.
pub(crate) fn downrated(panel_period_ns: i64, asked_period_ns: i64) -> bool {
    asked_period_ns > 0 && panel_period_ns > 0 && panel_period_ns * 2 >= asked_period_ns * 3
}

/// One second of a sink's present path.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Second {
    /// Presents handed to the display.
    pub(crate) released: u64,
    /// Presents confirmed on glass: ASC completions with a latch, SurfaceView `OnFrameRendered`.
    pub(crate) displays: u64,
    /// Decoded frames the store evicted before they presented.
    pub(crate) skipped: u64,
    /// Decoded frames the codec handed back.
    pub(crate) outputs: u64,
    /// Confirms come on this device: always on ASC, after the first `OnFrameRendered` on
    /// SurfaceView.
    pub(crate) confirms_live: bool,
    /// No loss and no parked-AU drop this second.
    pub(crate) unbroken: bool,
}

/// What a run of seconds proved.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Verdict {
    /// Presents go out and nothing reaches glass: the configuration failed.
    ShowsNothing,
    /// The store evicted `permille` of the decoded frames over the window.
    Skipping { permille: u64 },
}

/// The verdict streaks. Each verdict fires once, on the second its streak completes, and re-arms
/// after a second that breaks the streak.
#[derive(Debug, Default)]
pub(crate) struct Verdicts {
    dark: u32,
    skipping: u32,
    skipped: u64,
    outputs: u64,
}

impl Verdicts {
    /// Fold one second; `panel_period_ns` (0 = unknown) bounds the rate a skipping second may
    /// run at. A source above the panel rate must drop frames, so it never counts.
    pub(crate) fn second(&mut self, s: Second, panel_period_ns: i64) -> Option<Verdict> {
        let dark = s.confirms_live && s.released > 0 && s.displays == 0;
        self.dark = if dark { self.dark + 1 } else { 0 };
        // outputs ≤ 1.1 × the panel rate, in whole nanoseconds.
        let under_panel = panel_period_ns > 0
            && s.outputs.saturating_mul(panel_period_ns as u64 * 10) <= 11_000_000_000;
        if s.unbroken && under_panel && s.outputs > 0 && s.skipped * 5 >= s.outputs {
            self.skipping += 1;
            self.skipped += s.skipped;
            self.outputs += s.outputs;
        } else {
            (self.skipping, self.skipped, self.outputs) = (0, 0, 0);
        }
        if self.dark == DARK_SECONDS {
            return Some(Verdict::ShowsNothing);
        }
        (self.skipping == SKIPPING_SECONDS).then(|| Verdict::Skipping {
            permille: self.skipped * 1000 / self.outputs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P120: i64 = 8_333_333;
    const P60: i64 = 16_666_666;

    fn sec(released: u64, displays: u64, skipped: u64, outputs: u64) -> Second {
        Second {
            released,
            displays,
            skipped,
            outputs,
            confirms_live: true,
            unbroken: true,
        }
    }

    /// Every verdict a script of seconds returns, with the second it fired on.
    fn run(v: &mut Verdicts, script: &[Second], panel: i64) -> Vec<(usize, Verdict)> {
        script
            .iter()
            .enumerate()
            .filter_map(|(i, &s)| v.second(s, panel).map(|x| (i, x)))
            .collect()
    }

    #[test]
    fn two_seconds_of_presents_without_a_confirm_show_nothing_once() {
        let mut v = Verdicts::default();
        let dark = sec(120, 0, 0, 120);
        assert_eq!(
            run(&mut v, &[dark; 6], P120),
            [(1, Verdict::ShowsNothing)],
            "fires on the second dark second, then never again in that streak"
        );
        // A healthy second re-arms it.
        let script = [sec(120, 120, 0, 120), dark, dark];
        assert_eq!(run(&mut v, &script, P120), [(2, Verdict::ShowsNothing)]);
    }

    #[test]
    fn a_one_second_silence_a_stall_or_a_device_without_confirms_shows_nothing_never() {
        let mut v = Verdicts::default();
        let ok = sec(120, 120, 0, 120);
        let script = [
            ok,
            sec(120, 0, 0, 120),
            ok,
            sec(0, 0, 0, 0),
            sec(0, 0, 0, 0),
            ok,
        ];
        assert!(run(&mut v, &script, P120).is_empty());
        let unconfirmed = Second {
            confirms_live: false,
            ..sec(120, 0, 0, 120)
        };
        assert!(run(&mut v, &[unconfirmed; 10], P120).is_empty());
    }

    #[test]
    fn five_unbroken_seconds_evicting_a_fifth_at_panel_rate_are_skipping() {
        let mut v = Verdicts::default();
        let skipping = sec(90, 90, 30, 120);
        assert_eq!(
            run(&mut v, &[skipping; 8], P120),
            [(4, Verdict::Skipping { permille: 250 })]
        );
        // Exactly a fifth counts; one frame under does not.
        let mut v = Verdicts::default();
        assert_eq!(
            run(&mut v, &[sec(96, 96, 24, 120); 5], P120),
            [(4, Verdict::Skipping { permille: 200 })]
        );
        assert!(run(&mut v, &[sec(97, 97, 23, 120); 5], P120).is_empty());
    }

    #[test]
    fn a_lossy_second_inside_the_five_restarts_the_count() {
        let mut v = Verdicts::default();
        let skipping = sec(90, 90, 30, 120);
        let lossy = Second {
            unbroken: false,
            ..skipping
        };
        let mut script = vec![skipping; 4];
        script.push(lossy);
        script.extend([skipping; 4]);
        assert!(run(&mut v, &script, P120).is_empty());
        assert_eq!(
            run(&mut v, &[skipping], P120),
            [(0, Verdict::Skipping { permille: 250 })]
        );
    }

    #[test]
    fn healthy_replays_and_a_source_above_the_panel_rate_never_skip() {
        let healthy_120: Vec<Second> = (0..60).map(|i| sec(120, 120, i % 3, 120)).collect();
        let smooth_60 = [sec(60, 60, 0, 60); 60];
        let mut v = Verdicts::default();
        assert!(run(&mut v, &healthy_120, P120).is_empty());
        assert!(run(&mut v, &smooth_60, P120).is_empty());
        // 120 fps on a 60 Hz panel evicts half by design.
        assert!(run(&mut v, &[sec(60, 60, 60, 120); 10], P60).is_empty());
        // 10 % over the panel rate is the ceiling; an unknown panel judges nothing.
        assert!(run(&mut v, &[sec(52, 52, 14, 66); 4], P60).is_empty());
        assert_eq!(
            run(&mut v, &[sec(52, 52, 14, 66)], P60),
            [(0, Verdict::Skipping { permille: 212 })]
        );
        assert!(run(&mut v, &[sec(53, 53, 14, 67); 6], P60).is_empty());
        assert!(run(&mut v, &[sec(90, 90, 30, 120); 6], 0).is_empty());
    }

    #[test]
    fn a_panel_at_one_and_a_half_times_the_asked_period_is_downrated() {
        assert!(downrated(P60, P120), "60 Hz under a 120 Hz ask");
        assert!(!downrated(P120, P120));
        assert!(!downrated(11_000_000, P120), "a 90 Hz panel is under 3/2");
        assert!(!downrated(P60, 0), "no ask, no rule");
        assert!(!downrated(0, P120), "no panel period yet");
    }

    #[test]
    fn a_real_60_hz_panel_under_a_120_fps_stream_is_not_downrated() {
        // The mode table has no faster mode at the stream's size, so Kotlin asks for 60 Hz.
        assert!(!downrated(P60, P60));
    }
}
