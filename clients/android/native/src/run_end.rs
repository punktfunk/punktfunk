//! What follows one MediaCodec decode run: how the run ended, and whether the supervisor
//! rebuilds after a failed one. No codec and no clock: the supervisor reports what happened
//! and acts on the answer. The supervisor is `run_async` in `decode/async_loop.rs`.

use crate::input_stall::RungFloor;

/// Failed runs in a row, on one ladder rung, whose decoder never reached
/// [`BARREN_FRAMES`] before the loop gives up. Past this the device keeps refusing the codec,
/// and further rebuilds would only churn it.
pub(crate) const MAX_BARREN_REBUILDS: u32 = 3;

/// Frames a failed run must have shown to count as a decoder that worked. Half a second at
/// 60 Hz: a codec that dies on its first frames is the configuration, not the link, and a
/// frame or two before the death would otherwise reset the count forever.
pub(crate) const BARREN_FRAMES: u64 = 30;

/// How a run ended. `F` is the frame that starts the next run on a format change.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ending<F> {
    /// The session stopped video, or there was nothing to rebuild.
    Stopped,
    /// The host changed the stream's format at `F`.
    Reformat(F),
    /// The codec died or stopped taking input.
    Failed,
}

/// The ending of a run whose loop stopped. `failed`: the codec died, hung or showed nothing,
/// or a rebuild found no codec. A stopped session outranks everything, a format change a failure.
pub(crate) fn ending<F>(reformat: Option<F>, failed: bool, shutdown: bool) -> Ending<F> {
    match reformat {
        Some(f) if !shutdown => Ending::Reformat(f),
        _ if failed && !shutdown => Ending::Failed,
        _ => Ending::Stopped,
    }
}

/// What the supervisor does after a failed run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    /// Build another codec. `stepped_down`: the floor moved to a lower rung.
    Rebuild { stepped_down: bool },
    /// The device keeps refusing the codec: move to another, else leave the video frozen.
    GiveUp,
}

/// A run failed after `rendered` frames. `barren` counts the failed runs in a row that never
/// reached [`BARREN_FRAMES`]; a step down gives the lower rung its own tries. `clean`,
/// `lower`: see [`RungFloor::note_failed`].
pub(crate) fn after_failure(
    barren: &mut u32,
    floor: &mut RungFloor,
    rendered: u64,
    clean: bool,
    lower: Option<usize>,
) -> Retry {
    *barren = if rendered >= BARREN_FRAMES {
        0
    } else {
        *barren + 1
    };
    let stepped_down = floor.note_failed(rendered, clean, lower);
    if stepped_down {
        *barren = 0;
    }
    if *barren >= MAX_BARREN_REBUILDS {
        Retry::GiveUp
    } else {
        Retry::Rebuild { stepped_down }
    }
}

#[cfg(test)]
mod tests {
    use super::{after_failure, ending, Ending, Retry, BARREN_FRAMES, MAX_BARREN_REBUILDS};
    use crate::input_stall::{RungFloor, SETTLED_FRAMES};

    #[test]
    fn a_stopped_session_outranks_a_format_change_and_a_format_change_a_failure() {
        let f = Some("frame");
        // (reformat, failed, shutdown, ending)
        let rows = [
            (None, false, false, Ending::Stopped),
            (None, true, false, Ending::Failed),
            (f, false, false, Ending::Reformat("frame")),
            (f, true, false, Ending::Reformat("frame")),
            (None, false, true, Ending::Stopped),
            (None, true, true, Ending::Stopped),
            (f, false, true, Ending::Stopped),
            (f, true, true, Ending::Stopped),
        ];
        for (reformat, failed, shutdown, want) in rows {
            assert_eq!(
                ending(reformat, failed, shutdown),
                want,
                "{reformat:?} {failed} {shutdown}"
            );
        }
    }

    /// Runs `(rendered, clean, lower)` through one floor, as the supervisor does.
    fn retries(runs: &[(u64, bool, Option<usize>)]) -> (Vec<Retry>, usize) {
        let (mut barren, mut floor) = (0, RungFloor::default());
        let out = runs
            .iter()
            .map(|&(rendered, clean, lower)| {
                after_failure(&mut barren, &mut floor, rendered, clean, lower)
            })
            .collect();
        (out, floor.rung())
    }

    #[test]
    fn decoders_that_show_nothing_are_given_up_on_at_the_ladders_end() {
        let (out, _) = retries(&[(0, false, None); MAX_BARREN_REBUILDS as usize]);
        let rebuild = Retry::Rebuild {
            stepped_down: false,
        };
        assert_eq!(out, [rebuild, rebuild, Retry::GiveUp]);
    }

    /// A frame or two before the death is not a working decoder; `BARREN_FRAMES` is.
    #[test]
    fn a_decoder_that_showed_a_steady_picture_restarts_the_count() {
        let (out, _) = retries(&[(0, false, None), (0, false, None), (1, false, None)]);
        assert_eq!(out.last(), Some(&Retry::GiveUp), "one frame is not health");
        let (out, _) = retries(&[
            (0, false, None),
            (0, false, None),
            (BARREN_FRAMES, false, None),
            (0, false, None),
            (0, false, None),
        ]);
        assert!(out.iter().all(|r| *r != Retry::GiveUp));
    }

    /// Two clean failures move the floor, and the lower rung gets the full count again.
    #[test]
    fn a_step_down_gives_the_lower_rung_its_own_tries() {
        let step = Retry::Rebuild { stepped_down: true };
        let stay = Retry::Rebuild {
            stepped_down: false,
        };
        let (out, rung) = retries(&[
            (0, true, Some(2)),
            (0, true, Some(2)),
            (0, true, None),
            (0, true, None),
            (0, true, None),
        ]);
        assert_eq!(out, [stay, step, stay, stay, Retry::GiveUp]);
        assert_eq!(rung, 2);
    }

    /// A failure after a loss never moves the floor, so the count alone ends the loop.
    #[test]
    fn unclean_failures_never_step_down() {
        let (out, rung) = retries(&[(0, false, Some(1)); 3]);
        assert_eq!(out.last(), Some(&Retry::GiveUp));
        assert_eq!(rung, 0);
    }

    /// A rung that once showed [`SETTLED_FRAMES`] frames keeps its place.
    #[test]
    fn a_settled_rung_is_rebuilt_in_place() {
        let (out, rung) = retries(&[
            (SETTLED_FRAMES, true, Some(1)),
            (0, true, Some(1)),
            (0, true, Some(1)),
        ]);
        assert_eq!(
            out,
            [Retry::Rebuild {
                stepped_down: false
            }; 3]
        );
        assert_eq!(rung, 0);
    }
}
