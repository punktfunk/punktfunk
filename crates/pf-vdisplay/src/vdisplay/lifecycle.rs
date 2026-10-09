//! Per-display lifecycle: earned refcount, linger, and pin.
//!
//! Pure: no I/O, no OS types. [`State`] reports [`Acquire`] / [`Release`];
//! the owner of the backend resource applies those: [`super::registry`] on
//! Linux, the Windows manager's slots on Windows.
//!
//! Contract: `Idle → Active{refs} → Lingering{until} | Pinned → Idle`.
//! `Pinned` never expires here; only [`State::force_release`] drops it.
//!
//! Pin with the property test below. Design: `design/display-management.md`.

use std::time::Instant;

use super::policy::Linger;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Idle,
    Active {
        refs: u32,
    },
    Lingering {
        until: Instant,
    },
    Pinned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acquire {
    Create,
    Join,
    Reuse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    Decref,
    Linger,
    Pin,
    Teardown,
    Noop,
}

/// Linger applied on release. A deliberate quit (`force_immediate`) turns a
/// linger window into Immediate. `Forever` outranks quit: the screen stays
/// until `/display/release`.
pub fn effective_linger(force_immediate: bool, policy: Linger) -> Linger {
    match (force_immediate, policy) {
        (true, Linger::Forever) => Linger::Forever,
        (true, _) => Linger::Immediate,
        (false, l) => l,
    }
}

impl State {
    // Tests pin the resource-alive invariant; not production API.
    #[allow(dead_code)]
    pub fn has_display(self) -> bool {
        !matches!(self, State::Idle)
    }

    /// Live holds: the `/display/state` session count.
    pub fn refs(self) -> u32 {
        match self {
            State::Active { refs } => refs,
            _ => 0,
        }
    }

    /// The `/display/state` wire name. Idle is never listed.
    pub fn label(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Active { .. } => "active",
            State::Lingering { .. } => "lingering",
            State::Pinned => "pinned",
        }
    }

    pub fn acquire(&mut self) -> Acquire {
        match *self {
            State::Idle => {
                *self = State::Active { refs: 1 };
                Acquire::Create
            }
            State::Active { refs } => {
                *self = State::Active { refs: refs + 1 };
                Acquire::Join
            }
            State::Lingering { .. } | State::Pinned => {
                *self = State::Active { refs: 1 };
                Acquire::Reuse
            }
        }
    }

    pub fn release(&mut self, now: Instant, linger: Linger) -> Release {
        match *self {
            State::Active { refs } if refs > 1 => {
                *self = State::Active { refs: refs - 1 };
                Release::Decref
            }
            State::Active { .. } => match linger {
                Linger::Immediate => {
                    *self = State::Idle;
                    Release::Teardown
                }
                Linger::For(d) => {
                    *self = State::Lingering { until: now + d };
                    Release::Linger
                }
                Linger::Forever => {
                    *self = State::Pinned;
                    Release::Pin
                }
            },
            // Gen-stamped leases already no-op a stale drop; this is the backstop.
            State::Idle | State::Lingering { .. } | State::Pinned => Release::Noop,
        }
    }

    /// `Pinned` has no deadline; only Lingering expires.
    pub fn poll_expiry(&mut self, now: Instant) -> bool {
        match *self {
            State::Lingering { until } if now >= until => {
                *self = State::Idle;
                true
            }
            _ => false,
        }
    }

    /// Active is refused: live sessions are not display-managed.
    pub fn force_release(&mut self) -> bool {
        match *self {
            State::Lingering { .. } | State::Pinned => {
                *self = State::Idle;
                true
            }
            State::Active { .. } | State::Idle => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn create_join_reuse_and_teardown() {
        let mut s = State::default();
        assert_eq!(s.acquire(), Acquire::Create);
        assert_eq!(s, State::Active { refs: 1 });
        assert_eq!(s.acquire(), Acquire::Join);
        assert_eq!(s.refs(), 2);
        let now = Instant::now();
        assert_eq!(s.release(now, Linger::Immediate), Release::Decref);
        assert_eq!(s.refs(), 1);
        assert_eq!(s.release(now, Linger::Immediate), Release::Teardown);
        assert_eq!(s, State::Idle);
        assert!(!s.has_display());
    }

    #[test]
    fn linger_then_reuse_within_window() {
        let mut s = State::default();
        let t0 = Instant::now();
        s.acquire();
        assert_eq!(
            s.release(t0, Linger::For(Duration::from_secs(10))),
            Release::Linger
        );
        assert!(s.has_display());
        assert!(!s.poll_expiry(t0 + Duration::from_secs(5)));
        assert_eq!(s.acquire(), Acquire::Reuse);
        assert_eq!(s, State::Active { refs: 1 });
    }

    #[test]
    fn linger_expires_to_teardown() {
        let mut s = State::default();
        let t0 = Instant::now();
        s.acquire();
        s.release(t0, Linger::For(Duration::from_secs(10)));
        assert!(s.poll_expiry(t0 + Duration::from_secs(11)));
        assert_eq!(s, State::Idle);
        assert!(!s.poll_expiry(t0 + Duration::from_secs(12)));
    }

    #[test]
    fn pinned_never_expires_but_force_releases() {
        let mut s = State::default();
        let t0 = Instant::now();
        s.acquire();
        assert_eq!(s.release(t0, Linger::Forever), Release::Pin);
        assert_eq!(s, State::Pinned);
        assert!(!s.poll_expiry(t0 + Duration::from_secs(86_400)));
        assert!(s.has_display());
        assert!(s.force_release());
        assert_eq!(s, State::Idle);
    }

    #[test]
    fn force_release_refuses_active() {
        let mut s = State::default();
        s.acquire();
        assert!(
            !s.force_release(),
            "an active display can't be force-released"
        );
        assert_eq!(s.refs(), 1);
        let mut idle = State::default();
        assert!(!idle.force_release());
    }

    #[test]
    fn deliberate_quit_skips_the_linger_window_but_never_a_pin() {
        let ten = Linger::For(Duration::from_secs(10));
        assert_eq!(effective_linger(true, ten), Linger::Immediate);
        assert_eq!(effective_linger(true, Linger::Immediate), Linger::Immediate);
        assert_eq!(effective_linger(true, Linger::Forever), Linger::Forever);
        assert_eq!(effective_linger(false, ten), ten);
        assert_eq!(effective_linger(false, Linger::Forever), Linger::Forever);
    }

    /// `/display/state` clients match on these strings on both OSes.
    #[test]
    fn labels_are_the_display_state_wire_names() {
        let now = Instant::now();
        assert_eq!(State::Active { refs: 2 }.label(), "active");
        assert_eq!(State::Lingering { until: now }.label(), "lingering");
        assert_eq!(State::Pinned.label(), "pinned");
        assert_eq!(State::Idle.label(), "idle");
    }

    #[test]
    fn stale_release_is_noop() {
        let mut s = State::default();
        assert_eq!(s.release(Instant::now(), Linger::Immediate), Release::Noop);
        assert_eq!(s, State::Idle);
    }

    /// Seeded walk of acquire / release / tick / force-release.
    ///
    /// `has_display()` matches a shadow resource flag (no leak, no double-free).
    /// `refs()` matches live holds (no underflow). Expiry and force-release
    /// never tear down a held display.
    #[test]
    fn property_no_leaks_no_double_free_no_underflow() {
        // Knuth MMIX LCG. Seeded walk; no extra crate.
        let mut rng: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (rng >> 33) as u32
        };

        let base = Instant::now();
        let mut logical_ms: u64 = 0;
        let mut s = State::default();
        let mut resource_alive = false;
        let mut live_holds: u32 = 0;

        for _ in 0..200_000 {
            // 0..=1999 ms so a For(linger) can expire mid-walk.
            logical_ms += (next() % 2000) as u64;
            let now = base + Duration::from_millis(logical_ms);

            match next() % 5 {
                0 => {
                    let before_alive = resource_alive;
                    let a = s.acquire();
                    match a {
                        Acquire::Create => {
                            assert!(!before_alive, "Create while a resource was alive")
                        }
                        Acquire::Join | Acquire::Reuse => {
                            assert!(before_alive, "Join/Reuse with no live resource")
                        }
                    }
                    resource_alive = true;
                    live_holds += 1;
                }
                1 | 2 => {
                    // Weighted 2/5 so refs drain.
                    let linger = match next() % 3 {
                        0 => Linger::Immediate,
                        1 => Linger::For(Duration::from_millis((next() % 3000) as u64 + 1)),
                        _ => Linger::Forever,
                    };
                    let held_before = live_holds;
                    let r = s.release(now, linger);
                    match r {
                        Release::Noop => assert_eq!(held_before, 0, "Noop only with no live hold"),
                        Release::Decref => {
                            assert!(held_before >= 2, "Decref must leave the display held");
                            live_holds -= 1;
                        }
                        Release::Teardown => {
                            assert_eq!(held_before, 1, "Teardown only on the last hold");
                            live_holds = 0;
                            resource_alive = false;
                        }
                        Release::Linger | Release::Pin => {
                            assert_eq!(held_before, 1, "Linger/Pin only on the last hold");
                            live_holds = 0;
                            // Resource stays; only the hold count goes to 0.
                        }
                    }
                }
                3 => {
                    if s.poll_expiry(now) {
                        assert_eq!(live_holds, 0, "expiry tore down a held display");
                        resource_alive = false;
                    }
                }
                _ => {
                    if s.force_release() {
                        assert_eq!(live_holds, 0, "force-release tore down a held display");
                        resource_alive = false;
                    }
                }
            }

            assert_eq!(
                s.has_display(),
                resource_alive,
                "has_display drifted from the shadow model"
            );
            assert_eq!(
                s.refs(),
                live_holds,
                "refs drifted from the live-hold count"
            );
        }
    }
}
