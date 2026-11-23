//! Whether one process's audio is a copy of everything else that plays.
//!
//! A tool that sets a volume per app takes each app's audio and plays it again from a process
//! of its own. A capture that holds both the app and that process carries every sound twice,
//! the copy a moment late. No list of such tools is complete, so a process is judged by what
//! it plays: its audio is compared with the rest at every delay up to [`MAX_LAG`], and a
//! process whose audio follows the rest is a replayer. The app it copies is not one: its
//! audio comes first.
//!
//! Pure: samples in, a verdict out. The capture backend feeds it and acts on the verdict.

/// The rate both signals are compared at. Speech and music carry most of their energy below
/// half of it.
pub const RATE_HZ: u32 = 8_000;
/// One comparison: half a second of the candidate. A multiple of [`LANES`].
const WINDOW: usize = RATE_HZ as usize / 2;
/// The longest delay a copy is looked for at: a quarter of a second.
const MAX_LAG: usize = RATE_HZ as usize / 4;
/// A replay is at least half a millisecond late. Closer is one sound played twice in step.
const MIN_LAG: usize = RATE_HZ as usize / 2000;
/// New samples between two comparisons.
const HOP: usize = WINDOW / 2;
/// Below this RMS (-120 dBFS) a window is digital silence and says nothing.
const QUIET: f32 = 1e-6;
/// A window whose best match reaches this is a copy. One app of three, replayed alone, is 0.58.
const COPY: f32 = 0.5;
/// Two copies agree when their delays are within a millisecond.
const SAME_LAG: usize = RATE_HZ as usize / 1000;
/// This many windows of the candidate playing, none a copy, clear it: three seconds.
const CLEAR_AFTER: u32 = 12;
/// The history a comparison reads, and what [`Judge`] keeps before it drops the oldest.
const NEED: usize = WINDOW + MAX_LAG;
const KEEP: usize = 2 * (NEED + HOP);

/// What a candidate plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The rest of the audio, again.
    Replays,
    /// Audio of its own.
    Own,
}

/// Mono at about [`RATE_HZ`] from interleaved stereo: the mean of each run of frames.
pub struct Decimator {
    step: usize,
    sum: f32,
    held: usize,
}

impl Decimator {
    pub fn new(source_hz: u32) -> Decimator {
        Decimator {
            step: (source_hz / RATE_HZ).max(1) as usize,
            sum: 0.0,
            held: 0,
        }
    }

    /// Appends what `stereo` completes. A run that spans two calls finishes in the second.
    pub fn push(&mut self, stereo: &[f32], out: &mut Vec<f32>) {
        for frame in stereo.chunks_exact(2) {
            self.sum += (frame[0] + frame[1]) * 0.5;
            self.held += 1;
            if self.held == self.step {
                out.push(self.sum / self.step as f32);
                self.sum = 0.0;
                self.held = 0;
            }
        }
    }
}

/// One candidate's audio beside the rest, compared every [`HOP`] samples.
pub struct Judge {
    rest: Vec<f32>,
    own: Vec<f32>,
    /// Samples since the last comparison.
    fresh: usize,
    /// The delay of the last window, if it held a copy.
    copy_at: Option<usize>,
    cleared: u32,
    verdict: Option<Verdict>,
}

impl Default for Judge {
    /// Holds all the history it will ever keep, so feeding it never allocates.
    fn default() -> Judge {
        Judge {
            rest: Vec::with_capacity(KEEP + RATE_HZ as usize),
            own: Vec::with_capacity(KEEP + RATE_HZ as usize),
            fresh: 0,
            copy_at: None,
            cleared: 0,
            verdict: None,
        }
    }
}

impl Judge {
    /// Takes the same span of both signals at [`RATE_HZ`]: everyone else's audio and the
    /// candidate's. Answers once the candidate is known, and keeps answering the same.
    pub fn feed(&mut self, rest: &[f32], own: &[f32]) -> Option<Verdict> {
        if self.verdict.is_some() {
            return self.verdict;
        }
        let n = rest.len().min(own.len());
        self.rest.extend_from_slice(&rest[..n]);
        self.own.extend_from_slice(&own[..n]);
        self.fresh += n;
        // Each window ends `fresh` samples before the newest one, so every hop is judged once.
        while self.verdict.is_none() && self.fresh >= HOP {
            self.fresh -= HOP;
            let end = self.rest.len() - self.fresh;
            if end >= NEED {
                self.judge(end);
            }
        }
        if self.rest.len() > KEEP {
            let cut = self.rest.len() - (NEED + HOP);
            self.rest.drain(..cut);
            self.own.drain(..cut);
        }
        self.verdict
    }

    /// Compares the candidate's window that ends at `end` with the rest, as late and earlier.
    fn judge(&mut self, end: usize) {
        let own = &self.own[end - WINDOW..end];
        let own_energy = dot(own, own);
        if rms(own_energy) < QUIET {
            return;
        }
        let (mut best, mut best_lag) = (0.0f32, 0);
        // f64: a running sum in f32 keeps the residue of a loud passage after it has left.
        let mut rest_energy = f64::from(dot(
            &self.rest[end - WINDOW..end],
            &self.rest[end - WINDOW..end],
        ));
        for lag in 0..=MAX_LAG {
            let start = end - WINDOW - lag;
            if lag > 0 {
                // One sample enters at the old end and one leaves at the new end.
                let (gained, lost) = (self.rest[start], self.rest[start + WINDOW]);
                rest_energy += f64::from(gained * gained) - f64::from(lost * lost);
            }
            let rest_energy = rest_energy.max(0.0) as f32;
            if rms(rest_energy) < QUIET {
                continue;
            }
            let matched =
                dot(own, &self.rest[start..start + WINDOW]) / (own_energy * rest_energy).sqrt();
            if matched > best {
                (best, best_lag) = (matched, lag);
            }
        }
        if best >= COPY && best_lag >= MIN_LAG {
            if self
                .copy_at
                .is_some_and(|at| at.abs_diff(best_lag) <= SAME_LAG)
            {
                self.verdict = Some(Verdict::Replays);
            }
            self.copy_at = Some(best_lag);
        } else {
            // It played and nothing earlier in the rest matches: nothing to copy from.
            self.copy_at = None;
            self.cleared += 1;
            if self.cleared >= CLEAR_AFTER {
                self.verdict = Some(Verdict::Own);
            }
        }
    }
}

/// Running sums the compiler can keep in vector lanes; one sum is a serial chain.
const LANES: usize = 8;

/// The dot product of two [`WINDOW`]s.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; LANES];
    for (x, y) in a.chunks_exact(LANES).zip(b.chunks_exact(LANES)) {
        for i in 0..LANES {
            acc[i] += x[i] * y[i];
        }
    }
    acc.iter().sum()
}

fn rms(window_energy: f32) -> f32 {
    (window_energy / WINDOW as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Band-limited noise at [`RATE_HZ`]: deterministic, with the long tail real audio has.
    fn noise(seed: u32, len: usize) -> Vec<f32> {
        let mut state = seed;
        let mut low = 0.0f32;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let white = (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                low += 0.3 * (white - low);
                low * 0.6
            })
            .collect()
    }

    fn delayed(x: &[f32], by: usize, gain: f32) -> Vec<f32> {
        let mut out = vec![0.0; by];
        out.extend(x.iter().map(|v| v * gain));
        out.truncate(x.len());
        out
    }

    fn sum(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(x, y)| x + y).collect()
    }

    /// Feeds both signals in chunks of an IO cycle and returns the first verdict.
    fn run(rest: &[f32], own: &[f32]) -> Option<Verdict> {
        let mut judge = Judge::default();
        rest.chunks(85)
            .zip(own.chunks(85))
            .find_map(|(r, o)| judge.feed(r, o))
    }

    const SECOND: usize = RATE_HZ as usize;

    #[test]
    fn a_copy_tens_of_milliseconds_late_replays() {
        let apps = noise(1, SECOND * 2);
        // 57 ms, at 0.7 of the level.
        assert_eq!(
            run(&apps, &delayed(&apps, 456, 0.7)),
            Some(Verdict::Replays)
        );
    }

    #[test]
    fn a_copy_a_few_milliseconds_late_replays() {
        let apps = noise(2, SECOND * 2);
        assert_eq!(run(&apps, &delayed(&apps, 22, 1.0)), Some(Verdict::Replays));
    }

    #[test]
    fn a_copy_of_one_app_among_two_replays() {
        let (a, b) = (noise(3, SECOND * 2), noise(4, SECOND * 2));
        assert_eq!(
            run(&sum(&a, &b), &delayed(&a, 240, 1.0)),
            Some(Verdict::Replays)
        );
    }

    #[test]
    fn a_copy_at_inaudible_level_replays() {
        let apps: Vec<f32> = noise(5, SECOND * 2).iter().map(|v| v * 3e-5).collect();
        assert_eq!(
            run(&apps, &delayed(&apps, 170, 0.5)),
            Some(Verdict::Replays)
        );
    }

    #[test]
    fn audio_of_its_own_is_cleared() {
        assert_eq!(
            run(&noise(6, SECOND * 5), &noise(7, SECOND * 5)),
            Some(Verdict::Own)
        );
    }

    #[test]
    fn the_app_a_replayer_copies_is_cleared() {
        // The candidate plays first; the rest holds its copy 21 ms later and another app.
        let app = noise(8, SECOND * 5);
        let rest = sum(&delayed(&app, 168, 0.8), &noise(9, SECOND * 5));
        assert_eq!(run(&rest, &app), Some(Verdict::Own));
    }

    #[test]
    fn one_sound_played_in_step_is_cleared() {
        let both = noise(10, SECOND * 5);
        assert_eq!(run(&both, &both), Some(Verdict::Own));
    }

    #[test]
    fn a_candidate_playing_into_silence_is_cleared() {
        assert_eq!(
            run(&vec![0.0; SECOND * 5], &noise(11, SECOND * 5)),
            Some(Verdict::Own)
        );
    }

    #[test]
    fn a_silent_candidate_stays_unknown() {
        assert_eq!(run(&noise(12, SECOND * 5), &vec![0.0; SECOND * 5]), None);
    }

    #[test]
    fn a_quiet_rest_after_a_loud_one_is_not_a_copy() {
        // The rest falls from full scale to a whisper while the candidate plays its own audio.
        let mut rest = noise(13, SECOND * 5);
        rest[SECOND..].iter_mut().for_each(|v| *v *= 1e-4);
        assert_eq!(run(&rest, &noise(14, SECOND * 5)), Some(Verdict::Own));
    }

    /// What one comparison costs the IO thread that feeds the judge. Run by hand:
    /// `cargo test -p pf-audio --release -- --ignored --nocapture a_comparison_takes`
    #[test]
    #[ignore = "a timing, not a check"]
    fn a_comparison_takes() {
        // Unrelated signals never settle early, so every hop is compared until it clears.
        let (rest, own) = (noise(15, SECOND * 3), noise(16, SECOND * 3));
        let mut judge = Judge::default();
        let start = std::time::Instant::now();
        judge.feed(&rest, &own);
        let hops = (SECOND * 3 - NEED) / HOP + 1;
        eprintln!(
            "{} us a comparison",
            start.elapsed().as_micros() / hops as u128
        );
    }

    #[test]
    fn the_decimator_averages_runs_across_calls() {
        let mut d = Decimator::new(48_000);
        let mut out = Vec::new();
        // Six frames of 1.0 make one sample, whichever call ends the run.
        d.push(&[1.0; 8], &mut out);
        assert!(out.is_empty());
        d.push(&[1.0; 4], &mut out);
        assert_eq!(out, vec![1.0]);
    }
}
