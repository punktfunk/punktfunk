//! The swap-chain drain worker's decisions, without the DDIs.
//!
//! The driver's `swap_chain_processor.rs` maps each DDI result into a [`Seen`], calls [`step`]
//! and does the [`Next`] it gets back. The DDI calls, their order and the acquire window (offer,
//! then `FinishedProcessingFrame`, nothing between) stay in that shell; this module only says
//! which one comes next.
//!
//! Every road back to an acquire passes the loop top, so a stop, a removed device or a gone
//! monitor is seen between any two frames, even while DWM composes without a pause.

/// The loop top's reads, in the order they win.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// The processor is dropping, or the OS unassigned the swap-chain.
    pub terminate: bool,
    /// A worker or the pool saw this render device removed.
    pub device_removed: bool,
    /// The weak link to the owning monitor no longer upgrades.
    pub monitor_gone: bool,
}

/// What `IddCxSwapChainReleaseAndAcquireBuffer2` returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acquire {
    /// `E_PENDING`: the chain is valid and DWM composed nothing new.
    Pending,
    /// A surface, with the OS's display stamp and frame number.
    Frame { display_qpc: u64, number: u32 },
    /// Any other failure: the chain is abandoned.
    Lost,
}

/// How the idle wait on the wake and surface-available events ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    Wake,
    Surface,
    /// The heartbeat interval passed with nothing signalled.
    Timeout,
    /// Anything else, with the raw wait result.
    Failed(u32),
}

/// What the shell observed since its last step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
    Top(Flags),
    Acquired(Acquire),
    Waited(Wait),
    /// The acquire window closed. `ok`: `FinishedProcessingFrame` succeeded. `hold`: the
    /// encoder reads the surface in place, so it stays the driver's until the next frame.
    Finished {
        ok: bool,
        hold: bool,
    },
}

/// The shell's next action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Read the [`Flags`].
    Top,
    /// Release the last frame and acquire the next.
    Acquire,
    /// Stamp the drain heartbeat, then wait for a wake, a surface or the heartbeat timeout.
    Wait,
    /// Keep the held surface as the pool's still picture, then the loop top.
    Retain,
    /// The acquire window: offer the surface, then `FinishedProcessingFrame`. `queued`: no
    /// empty acquire came since the last frame, so this one was already waiting.
    Offer {
        display_qpc: u64,
        number: u32,
        queued: bool,
    },
    /// Stamp the finished frame, wait out the encoder's read when `hold`, then the loop top.
    Finish {
        hold: bool,
    },
    Exit(Why),
}

/// Why the drain loop ends; the shell then deletes the swap-chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    Terminate,
    DeviceRemoved,
    MonitorGone,
    WaitFailed(u32),
    /// An acquire or a `FinishedProcessingFrame` failed: the OS abandoned the chain.
    Lost,
}

/// What the drain loop carries from one pass to the next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drain {
    /// An acquire came back empty since the last frame.
    pended: bool,
    /// The last frame's surface is still the driver's: a still desktop retains it.
    held: bool,
}

/// One decision of the drain loop.
#[must_use]
pub fn step(drain: &mut Drain, seen: Seen) -> Next {
    match seen {
        Seen::Top(f) if f.terminate => Next::Exit(Why::Terminate),
        Seen::Top(f) if f.device_removed => Next::Exit(Why::DeviceRemoved),
        Seen::Top(f) if f.monitor_gone => Next::Exit(Why::MonitorGone),
        Seen::Top(_) => Next::Acquire,
        Seen::Acquired(Acquire::Pending) => {
            drain.pended = true;
            Next::Wait
        }
        Seen::Acquired(Acquire::Frame {
            display_qpc,
            number,
        }) => {
            let queued = !drain.pended;
            drain.pended = false;
            drain.held = false;
            Next::Offer {
                display_qpc,
                number,
                queued,
            }
        }
        Seen::Acquired(Acquire::Lost) | Seen::Finished { ok: false, .. } => Next::Exit(Why::Lost),
        Seen::Waited(Wait::Failed(code)) => Next::Exit(Why::WaitFailed(code)),
        Seen::Waited(Wait::Timeout) if drain.held => {
            drain.held = false;
            Next::Retain
        }
        Seen::Waited(_) => Next::Top,
        Seen::Finished { ok: true, hold } => {
            drain.held = hold;
            Next::Finish { hold }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLEAR: Seen = Seen::Top(Flags {
        terminate: false,
        device_removed: false,
        monitor_gone: false,
    });
    const STOP: Seen = Seen::Top(Flags {
        terminate: true,
        device_removed: false,
        monitor_gone: false,
    });
    const PENDING: Seen = Seen::Acquired(Acquire::Pending);
    const FRAME: Seen = Seen::Acquired(Acquire::Frame {
        display_qpc: 7,
        number: 3,
    });

    fn offer(queued: bool) -> Next {
        Next::Offer {
            display_qpc: 7,
            number: 3,
            queued,
        }
    }

    fn finished(hold: bool) -> Seen {
        Seen::Finished { ok: true, hold }
    }

    /// Drives one fresh loop through `script`, each row an observation and the action it must
    /// produce. A script skips the top and the acquire where they change nothing.
    fn drive(script: &[(Seen, Next)]) {
        let mut drain = Drain::default();
        for (i, &(seen, want)) in script.iter().enumerate() {
            assert_eq!(step(&mut drain, seen), want, "row {i}: {seen:?}");
        }
    }

    #[test]
    fn terminate_wins_at_the_top_during_a_frame_burst() {
        // A frame never leads straight to another acquire.
        let burst = [
            (CLEAR, Next::Acquire),
            (FRAME, offer(true)),
            (finished(true), Next::Finish { hold: true }),
            (CLEAR, Next::Acquire),
            (FRAME, offer(true)),
            (finished(true), Next::Finish { hold: true }),
            (STOP, Next::Exit(Why::Terminate)),
        ];
        drive(&burst);
        // Even over a removed device and a gone monitor.
        let all = Seen::Top(Flags {
            terminate: true,
            device_removed: true,
            monitor_gone: true,
        });
        drive(&[(all, Next::Exit(Why::Terminate))]);
    }

    #[test]
    fn a_removed_device_exits_before_the_acquire() {
        let removed = Flags {
            device_removed: true,
            ..Flags::default()
        };
        drive(&[(Seen::Top(removed), Next::Exit(Why::DeviceRemoved))]);
        let both = Flags {
            monitor_gone: true,
            ..removed
        };
        drive(&[(Seen::Top(both), Next::Exit(Why::DeviceRemoved))]);
        let gone = Flags {
            monitor_gone: true,
            ..Flags::default()
        };
        drive(&[(Seen::Top(gone), Next::Exit(Why::MonitorGone))]);
    }

    #[test]
    fn a_timeout_retains_the_held_surface_once() {
        drive(&[
            (CLEAR, Next::Acquire),
            (FRAME, offer(true)),
            (finished(true), Next::Finish { hold: true }),
            (CLEAR, Next::Acquire),
            (PENDING, Next::Wait),
            (Seen::Waited(Wait::Timeout), Next::Retain),
            (CLEAR, Next::Acquire),
            (PENDING, Next::Wait),
            (Seen::Waited(Wait::Timeout), Next::Top),
        ]);
    }

    #[test]
    fn only_a_timeout_retains_and_only_a_held_surface() {
        // A copied frame holds nothing.
        drive(&[
            (FRAME, offer(true)),
            (finished(false), Next::Finish { hold: false }),
            (PENDING, Next::Wait),
            (Seen::Waited(Wait::Timeout), Next::Top),
        ]);
        // A wake or a surface keeps the hold for a later timeout.
        drive(&[
            (FRAME, offer(true)),
            (finished(true), Next::Finish { hold: true }),
            (PENDING, Next::Wait),
            (Seen::Waited(Wait::Wake), Next::Top),
            (PENDING, Next::Wait),
            (Seen::Waited(Wait::Surface), Next::Top),
            (PENDING, Next::Wait),
            (Seen::Waited(Wait::Timeout), Next::Retain),
        ]);
        // The next frame releases the held surface: nothing is left to retain.
        drive(&[
            (FRAME, offer(true)),
            (finished(true), Next::Finish { hold: true }),
            (FRAME, offer(true)),
            (finished(false), Next::Finish { hold: false }),
            (PENDING, Next::Wait),
            (Seen::Waited(Wait::Timeout), Next::Top),
        ]);
    }

    #[test]
    fn wake_and_surface_rerun_the_top() {
        drive(&[
            (PENDING, Next::Wait),
            (Seen::Waited(Wait::Wake), Next::Top),
            (STOP, Next::Exit(Why::Terminate)),
        ]);
        drive(&[
            (PENDING, Next::Wait),
            (Seen::Waited(Wait::Surface), Next::Top),
            (CLEAR, Next::Acquire),
        ]);
    }

    #[test]
    fn a_failed_wait_or_a_lost_chain_exits() {
        // A held surface is not retained on the way out.
        drive(&[
            (FRAME, offer(true)),
            (finished(true), Next::Finish { hold: true }),
            (PENDING, Next::Wait),
            (
                Seen::Waited(Wait::Failed(0x80)),
                Next::Exit(Why::WaitFailed(0x80)),
            ),
        ]);
        drive(&[
            (CLEAR, Next::Acquire),
            (Seen::Acquired(Acquire::Lost), Next::Exit(Why::Lost)),
        ]);
        drive(&[
            (FRAME, offer(true)),
            (
                Seen::Finished {
                    ok: false,
                    hold: true,
                },
                Next::Exit(Why::Lost),
            ),
        ]);
    }

    #[test]
    fn queued_means_no_empty_acquire_since_the_last_frame() {
        drive(&[
            // The first frame of a run counts as queued.
            (FRAME, offer(true)),
            (finished(false), Next::Finish { hold: false }),
            // Straight after a frame: queued.
            (FRAME, offer(true)),
            (finished(false), Next::Finish { hold: false }),
            // After one empty acquire, or several: waited for.
            (PENDING, Next::Wait),
            (Seen::Waited(Wait::Surface), Next::Top),
            (FRAME, offer(false)),
            (finished(false), Next::Finish { hold: false }),
            (PENDING, Next::Wait),
            (Seen::Waited(Wait::Timeout), Next::Top),
            (PENDING, Next::Wait),
            (Seen::Waited(Wait::Surface), Next::Top),
            (FRAME, offer(false)),
            (finished(false), Next::Finish { hold: false }),
            (FRAME, offer(true)),
        ]);
    }

    #[test]
    fn a_frame_hands_its_stamps_to_the_offer() {
        let mut drain = Drain::default();
        let seen = Seen::Acquired(Acquire::Frame {
            display_qpc: u64::MAX,
            number: 9,
        });
        assert_eq!(
            step(&mut drain, seen),
            Next::Offer {
                display_qpc: u64::MAX,
                number: 9,
                queued: true
            }
        );
    }
}
