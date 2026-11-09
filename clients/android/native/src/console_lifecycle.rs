//! The console's surface lifecycle: what its render thread does with Kotlin's window as the EGL
//! surface over it comes up, fails and goes. No EGL, Skia or clock: the loop in
//! `console/host.rs` reports each [`Event`], does the [`Action`] that comes back and reports
//! that result in turn. A run of [`FAILURE_LIMIT`] failures ends the thread with `Dead`, and
//! Kotlin's touch UI takes the screen. The table below pins each rule.

/// GL failures in a row the loop tolerates. One is a transient (a window torn down mid-create);
/// a run is a context that is not coming back, and a gray SurfaceView is worse than the touch UI.
pub(crate) const FAILURE_LIMIT: u32 = 3;

/// What the render thread saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// Kotlin handed over a window.
    Created,
    /// Kotlin's window is going.
    Destroyed,
    /// An EGL surface is up over the window.
    Attached,
    AttachFailed,
    /// Skia wrapped the EGL surface at its size.
    Wrapped,
    WrapFailed,
    SwapFailed,
    /// The end of a pass: Kotlin's commands applied, the frame drawn.
    Tick,
}

/// What the render thread does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Nothing,
    /// Put an EGL surface over the window.
    Attach,
    /// Drop the Skia surface, the EGL surface and the window.
    Detach,
    /// Detach and end the thread.
    Die,
}

#[derive(Default)]
pub(crate) struct Lifecycle {
    failures: u32,
}

impl Lifecycle {
    /// GL failures since the last good attach or wrap.
    pub(crate) fn failures(&self) -> u32 {
        self.failures
    }

    pub(crate) fn step(&mut self, ev: Event) -> Action {
        match ev {
            Event::Created => Action::Attach,
            Event::Destroyed | Event::SwapFailed => Action::Detach,
            Event::Attached | Event::Wrapped => {
                self.failures = 0;
                Action::Nothing
            }
            Event::AttachFailed => {
                self.failures += 1;
                Action::Detach
            }
            // The surface stays up and the next pass wraps again.
            Event::WrapFailed => {
                self.failures += 1;
                Action::Nothing
            }
            Event::Tick if self.failures >= FAILURE_LIMIT => Action::Die,
            Event::Tick => Action::Nothing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Action::{self, Attach, Detach, Die, Nothing};
    use super::Event::{
        self, AttachFailed, Attached, Created, Destroyed, SwapFailed, Tick, WrapFailed, Wrapped,
    };
    use super::Lifecycle;

    /// One script: each event with the action it answers and the failure run after it.
    type Case = (&'static str, &'static [(Event, Action, u32)]);

    const CASES: &[Case] = &[
        (
            "a window attaches and wraps",
            &[
                (Created, Attach, 0),
                (Attached, Nothing, 0),
                (Wrapped, Nothing, 0),
                (Tick, Nothing, 0),
            ],
        ),
        (
            "a destroy detaches",
            &[
                (Created, Attach, 0),
                (Attached, Nothing, 0),
                (Wrapped, Nothing, 0),
                (Destroyed, Detach, 0),
                (Tick, Nothing, 0),
            ],
        ),
        (
            "a failed attach drops the window and counts one",
            &[
                (Created, Attach, 0),
                (AttachFailed, Detach, 1),
                (Tick, Nothing, 1),
            ],
        ),
        (
            "three failed attaches in a row end the thread",
            &[
                (Created, Attach, 0),
                (AttachFailed, Detach, 1),
                (Tick, Nothing, 1),
                (Created, Attach, 1),
                (AttachFailed, Detach, 2),
                (Tick, Nothing, 2),
                (Created, Attach, 2),
                (AttachFailed, Detach, 3),
                (Tick, Die, 3),
            ],
        ),
        (
            "an attach restarts the run",
            &[
                (Created, Attach, 0),
                (AttachFailed, Detach, 1),
                (Created, Attach, 1),
                (AttachFailed, Detach, 2),
                (Created, Attach, 2),
                (Attached, Nothing, 0),
                (Tick, Nothing, 0),
            ],
        ),
        (
            "a failed wrap counts and the next pass wraps again",
            &[
                (Created, Attach, 0),
                (Attached, Nothing, 0),
                (WrapFailed, Nothing, 1),
                (Tick, Nothing, 1),
                (WrapFailed, Nothing, 2),
                (Tick, Nothing, 2),
                (WrapFailed, Nothing, 3),
                (Tick, Die, 3),
            ],
        ),
        (
            "a wrap restarts the run",
            &[
                (Created, Attach, 0),
                (Attached, Nothing, 0),
                (WrapFailed, Nothing, 1),
                (WrapFailed, Nothing, 2),
                (Wrapped, Nothing, 0),
                (Tick, Nothing, 0),
            ],
        ),
        (
            "a failed swap detaches uncounted and waits for the next window",
            &[
                (Created, Attach, 0),
                (Attached, Nothing, 0),
                (Wrapped, Nothing, 0),
                (SwapFailed, Detach, 0),
                (Tick, Nothing, 0),
                (Tick, Nothing, 0),
            ],
        ),
    ];

    #[test]
    fn the_lifecycle_table() {
        for &(name, steps) in CASES {
            let mut life = Lifecycle::default();
            for (i, &(ev, want, failures)) in steps.iter().enumerate() {
                let got = life.step(ev);
                assert_eq!(
                    (got, life.failures()),
                    (want, failures),
                    "{name}: step {i} ({ev:?})"
                );
            }
        }
    }
}
