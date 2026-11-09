//! The console's surface lifecycle: what its render thread does with Kotlin's window as the EGL
//! surface over it comes up, fails and goes. No EGL, Skia or clock: the loop in
//! `console/host.rs` reports each [`Event`], does the [`Action`] that comes back and reports
//! that result in turn. A run of [`FAILURE_LIMIT`] failures ends the thread with `Dead`, and
//! Kotlin's touch UI takes the screen. The table below pins each rule.
//!
//! [`Shared`] carries Kotlin's commands in and the thread's events out, and holds the destroy
//! handshake: Kotlin's `surfaceDestroyed` returns once the thread has let go of the window.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

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

/// The queues between Kotlin and the render thread, and the destroy handshake. `C` is what
/// Kotlin asks, `E` what the thread raises.
pub(crate) struct Shared<C, E> {
    inbox: Mutex<VecDeque<C>>,
    inbox_cv: Condvar,
    events: Mutex<VecDeque<E>>,
    events_cv: Condvar,
    /// Bumped by the render thread each time it has torn a surface down; a destroy waits for
    /// the bump.
    surface_gen: Mutex<u64>,
    surface_cv: Condvar,
    /// The shell's launch hold, as of the last frame. Written before the frame's events leave,
    /// so a `Launch` the shell raised is never read as unheld.
    holds_launch: AtomicBool,
    /// The render thread runs. Cleared as it ends: nothing then tears a surface down.
    alive: AtomicBool,
}

impl<C, E> Shared<C, E> {
    pub(crate) fn new() -> Self {
        Shared {
            inbox: Mutex::new(VecDeque::new()),
            inbox_cv: Condvar::new(),
            events: Mutex::new(VecDeque::new()),
            events_cv: Condvar::new(),
            surface_gen: Mutex::new(0),
            surface_cv: Condvar::new(),
            holds_launch: AtomicBool::new(false),
            alive: AtomicBool::new(true),
        }
    }

    /// Whether the shell's launch hold is up: its session waits for `ShowStream`.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub(crate) fn holds_launch(&self) -> bool {
        self.holds_launch.load(Ordering::Acquire)
    }

    /// The render thread's report of the hold, once a frame.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub(crate) fn set_holds_launch(&self, held: bool) {
        self.holds_launch.store(held, Ordering::Release);
    }

    pub(crate) fn send(&self, cmd: C) {
        self.inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(cmd);
        self.inbox_cv.notify_one();
    }

    pub(crate) fn emit(&self, ev: E) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(ev);
        self.events_cv.notify_one();
    }

    /// Kotlin's poll: the next event, waiting up to `timeout` for one.
    pub(crate) fn next_event(&self, timeout: Duration) -> Option<E> {
        let mut q = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if q.is_empty() {
            let (guard, _) = self
                .events_cv
                .wait_timeout(q, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            q = guard;
        }
        q.pop_front()
    }

    /// The render thread's take: everything queued. With `park`, an empty queue is waited on
    /// for up to half a second first.
    pub(crate) fn take(&self, park: bool) -> Vec<C> {
        let mut q = self
            .inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if park && q.is_empty() {
            let (guard, _) = self
                .inbox_cv
                .wait_timeout(q, Duration::from_millis(500))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            q = guard;
        }
        q.drain(..).collect()
    }

    /// Send `destroy` and wait (bounded) until the surface has gone. A render thread that has
    /// ended took its surface with it: nothing to wait for.
    pub(crate) fn destroy_surface_blocking(&self, destroy: C) {
        if !self.alive.load(Ordering::Acquire) {
            return;
        }
        let before = *self
            .surface_gen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.send(destroy);
        let g = self
            .surface_gen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Bounded: a render thread that died mid-frame must not hang the UI thread forever —
        // by then the EGL surface is gone with it anyway.
        let _ = self
            .surface_cv
            .wait_timeout_while(g, Duration::from_secs(2), |g| {
                *g == before && self.alive.load(Ordering::Acquire)
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    /// The render thread is ending: release a destroy that is waiting on it.
    pub(crate) fn retire(&self) {
        self.alive.store(false, Ordering::Release);
        self.ack_surface_gone();
    }

    pub(crate) fn ack_surface_gone(&self) {
        *self
            .surface_gen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        self.surface_cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::Action::{self, Attach, Detach, Die, Nothing};
    use super::Event::{
        self, AttachFailed, Attached, Created, Destroyed, SwapFailed, Tick, WrapFailed, Wrapped,
    };
    use super::{Lifecycle, Shared};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

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

    const DESTROY: u8 = 1;
    const GONE: u8 = 2;

    /// A render thread that takes commands until the destroy, then runs `then`.
    fn render_thread(
        shared: &Arc<Shared<u8, u8>>,
        then: fn(&Shared<u8, u8>),
    ) -> std::thread::JoinHandle<()> {
        let shared = shared.clone();
        std::thread::spawn(move || {
            while !shared.take(true).contains(&DESTROY) {}
            then(&shared);
        })
    }

    #[test]
    fn a_destroy_returns_once_the_thread_has_let_go() {
        let shared = Arc::new(Shared::new());
        let t = render_thread(&shared, |s| {
            std::thread::sleep(Duration::from_millis(20));
            s.emit(GONE);
            s.ack_surface_gone();
        });
        shared.destroy_surface_blocking(DESTROY);
        assert_eq!(shared.next_event(Duration::ZERO), Some(GONE));
        t.join().unwrap();
    }

    #[test]
    fn a_thread_that_ends_releases_the_destroy_waiting_on_it() {
        let shared = Arc::new(Shared::new());
        let t = render_thread(&shared, |s| {
            std::thread::sleep(Duration::from_millis(20));
            s.emit(GONE);
            s.retire();
        });
        let at = Instant::now();
        shared.destroy_surface_blocking(DESTROY);
        assert!(at.elapsed() < Duration::from_secs(1), "the bound ended it");
        assert_eq!(shared.next_event(Duration::ZERO), Some(GONE));
        t.join().unwrap();
    }

    #[test]
    fn a_destroy_after_the_thread_ended_sends_nothing_and_waits_for_nothing() {
        let shared = Shared::<u8, u8>::new();
        shared.retire();
        let at = Instant::now();
        shared.destroy_surface_blocking(DESTROY);
        assert!(at.elapsed() < Duration::from_secs(1));
        assert!(shared.take(false).is_empty());
    }
}
