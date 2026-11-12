//! What a rebuild changes and what follows it, decided before anything is built. [`plan`] is
//! pure: it reads an ask and [`RebuildFacts`], copies of what the loop runs, and says what to
//! build, at which mode and rate, which display request moves with it, which [`Epilogue`]
//! columns run on commit and what a failed build tells ([`Rollback`]). `None` rebuilds nothing.
//! `StreamState::rebuild` (`rebuild.rs`) performs a plan; it decides nothing a column says.
//!
//! Pinned by `each_ask_plans_its_rebuild`, one row per reason and per fix that set a column.

use super::session_watch::SessionSwitch;
use super::swap::SwapReason;
use crate::encode::Codec;
use crate::native::bitrate::pyrowave_mode_kbps;
use crate::session_plan::{CaptureBackend, SessionPlan};
use crate::vdisplay::Compositor;
use punktfunk_core::Mode;

/// Why the loop rebuilds, with what the ask carries.
pub(super) enum RebuildAsk {
    /// Gaming↔Desktop.
    SessionSwitch(SessionSwitch),
    /// The client's accepted mode, and the view it is encoded at (`None` = the mode).
    Mode(Mode, Option<(u32, u32)>),
    /// The client's codec ask, vetted by the control task.
    Format(Codec),
    /// The Windows topology watchdog's generation now.
    TopologyReassert(u64),
    /// The lost capturer's keepalive, when its display is still up.
    CaptureLost(Option<Box<dyn Send>>),
    /// The mode the source delivers, which the encoder was not opened at.
    SourceMode(Mode),
    /// A rate the encoder could not take in place, kbps.
    Bitrate(u32),
    /// Whether the source shows HDR content now, where it says: only gamescope does.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Content(bool),
}

/// What the loop runs when it plans.
#[derive(Clone, Copy, Debug)]
pub(super) struct RebuildFacts {
    /// The mode the session runs: the client's last ask, or what the source delivered.
    pub(super) mode: Mode,
    pub(super) rate_kbps: u32,
    pub(super) plan: SessionPlan,
    pub(super) compositor: Compositor,
    pub(super) bitrate_auto: bool,
    /// The session's PyroWave cap, kbps.
    pub(super) pyrowave_max_kbps: Option<u32>,
    /// The topology generation the loop last followed.
    pub(super) seen_reassert_gen: u64,
    /// No `PUNKTFUNK_COMPOSITOR` pin: a capture loss re-detects the live session, and input
    /// follows it. A pinned session leaves input to the operator.
    pub(super) follows_session: bool,
    /// A Windows IDD-push display can be mode-set in place.
    pub(super) windows: bool,
    /// Another session holds the display: its mode stays put.
    pub(super) shared: bool,
    /// The session streams a display it joined (`mode_conflict: join`).
    pub(super) joined: bool,
}

/// How the replacement is built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Build {
    /// One build on the live display, at the capturer's own first-frame wait.
    Once,
    /// [`super::pipeline::build_pipeline_with_retry`] over these attempts.
    Retry(std::ops::RangeInclusive<u32>),
    /// [`super::rebuild::rebuild_after_capture_loss`]: re-attach, then build until the budget
    /// runs out; only the first pass takes the short first-frame wait (S6-4).
    CaptureLoss,
    /// The encoder alone, on the live capture, at the delivered refresh.
    Encoder,
}

/// The display request a rebuild moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DisplayChange {
    /// A new display on this compositor, with the cursor plan and route it takes.
    Switch(Compositor),
    /// The plan re-resolved for this codec at the plan's mode; the live display takes its HDR
    /// verdict and cursor plan.
    Format(Codec),
    /// The plan re-resolved for content that turned HDR or SDR. The display stays lit as it
    /// is: only the capture reopens in the other range.
    Content(bool),
}

/// Who hears what became of the ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tell {
    /// `/status` and the client hear the delivered mode: on commit a correction when it is not
    /// the plan's mode, after a failed build always one.
    Mode,
    /// On commit, the delivered mode as a correction: the client holds a mode the source left.
    Correction,
    /// The control task hears the codec runs, or that its ask failed.
    Codec,
    /// After a failed build: a refused climb teaches the ceiling, a refused drop tells the
    /// client the rate kept (S7-4).
    Rate,
}

/// Whether the further screens restart on the session's new plan and display request (S8-3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Screens {
    /// They keep running; a first screen at a new mode only re-splits the shares (S8-5).
    Keep,
    Rebase,
    /// When the build moved the session to another compositor.
    RebaseIfMoved,
}

/// The rate the session runs after the commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rate {
    /// What the build opened at.
    Built,
    /// What the new encoder applied of the plan's rate; the ceiling learns it.
    Applied,
}

/// What a committed rebuild does past what the swap restarts. The open wire frame closes and
/// the gap is announced on every commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Epilogue {
    pub(super) tell: Option<Tell>,
    /// Input follows the compositor the stream lands on, only once it is there (S5-3).
    pub(super) input: bool,
    /// The portal cursor is read again off the display the stream lands on.
    pub(super) portal_cursor: bool,
    pub(super) screens: Screens,
    pub(super) rate: Rate,
    /// The new encoder is asked for a keyframe.
    pub(super) keyframe: bool,
    /// The topology generation is read again: capture rebuilt at the mode on an IDD-push
    /// display, so a bump the rebuild caused is no eviction (S5-9). An encoder-only rebuild
    /// re-attaches nothing, and a bump during it may be one.
    pub(super) reassert_sync: bool,
}

/// What a failed build does past restoring what prepare changed: the plan, depth, cursor plan
/// and display request. The frame open on the wire stays the running encoder's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rollback {
    /// The stall is announced as a pipeline gap (44692a0dc).
    pub(super) gap: bool,
    pub(super) tell: Option<Tell>,
    /// The session ends: nothing is left to stream.
    pub(super) ends: bool,
}

/// A full pipeline rebuild's epilogue, the base every row moves from.
const PIPELINE: Epilogue = Epilogue {
    tell: None,
    input: false,
    portal_cursor: false,
    screens: Screens::Keep,
    rate: Rate::Built,
    keyframe: false,
    reassert_sync: false,
};

/// A failed build the session survives.
const KEPT: Rollback = Rollback {
    gap: true,
    tell: None,
    ends: false,
};

/// A failed build that ends the session.
const ENDS: Rollback = Rollback { ends: true, ..KEPT };

/// What one rebuild changes and what follows it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct RebuildPlan {
    pub(super) reason: SwapReason,
    pub(super) build: Build,
    /// Try the Windows in-place mode set first; [`Self::build`] is its fallback.
    pub(super) in_place: bool,
    /// The mode built, and the session's mode after: the derivation runs at its refresh (S5-8).
    pub(super) mode: Mode,
    /// The wire budget built at, kbps.
    pub(super) rate_kbps: u32,
    pub(super) display: Option<DisplayChange>,
    /// The session-transition trace the rebuild stamps: `/status`'s last resize, and the gap.
    pub(super) trace: Option<&'static str>,
    /// A joiner alone on the display builds one of its own and stops joining.
    pub(super) stops_joining: bool,
    pub(super) epilogue: Epilogue,
    pub(super) rollback: Rollback,
}

/// The rate `mode` encoded at `view` runs at: PyroWave pins it per picture at the session's bits
/// per pixel, and an Automatic session keeps its rate inside the new bounds.
fn rate_at(mode: Mode, view: Option<(u32, u32)>, f: &RebuildFacts) -> u32 {
    if f.plan.codec != Codec::PyroWave {
        return f.rate_kbps;
    }
    let running = f.bitrate_auto.then_some(f.rate_kbps);
    let picture = crate::session_plan::picture(mode, view);
    pyrowave_mode_kbps(running, &picture, &f.plan, f.pyrowave_max_kbps)
}

/// The view a mode ask encodes once the plan built `built`: its own view where that is the
/// asked mode, else the asked size framed on the display that stays. A size at least the
/// display's on both axes frames nothing.
pub(super) fn view_for(asked: Mode, view: Option<(u32, u32)>, built: Mode) -> Option<(u32, u32)> {
    if asked == built {
        return view;
    }
    let (w, h) = view.unwrap_or((asked.width, asked.height));
    (w < built.width || h < built.height).then_some((w, h))
}

/// The rebuild `ask` needs, or `None`: a switch to the compositor that runs, the live mode and
/// view asked again (S5-6), on a shared display a size it already frames, a topology generation
/// already followed or no IDD-push to follow. A joiner alone on its display builds its own.
pub(super) fn plan(ask: &RebuildAsk, f: &RebuildFacts) -> Option<RebuildPlan> {
    let view = f.plan.reframe_to.map(|(_, v)| v);
    let idd_push = f.plan.capture == CaptureBackend::IddPush;
    let rebuilt = Epilogue {
        reassert_sync: idd_push,
        ..PIPELINE
    };
    let at_live = RebuildPlan {
        reason: SwapReason::ModeSwitch,
        build: Build::Once,
        in_place: false,
        mode: f.mode,
        rate_kbps: f.rate_kbps,
        display: None,
        trace: None,
        stops_joining: false,
        epilogue: rebuilt,
        rollback: KEPT,
    };
    let alone_on_joined = f.joined && !f.shared;
    Some(match *ask {
        RebuildAsk::SessionSwitch(ref sw) if sw.compositor == f.compositor => return None,
        RebuildAsk::SessionSwitch(ref sw) => RebuildPlan {
            reason: SwapReason::SessionSwitch,
            build: Build::Retry(1..=8),
            display: Some(DisplayChange::Switch(sw.compositor)),
            epilogue: Epilogue {
                input: true,
                portal_cursor: true,
                screens: Screens::Rebase,
                ..rebuilt
            },
            ..at_live
        },
        // Another session holds the display, so its mode stays: the encoder alone frames the
        // asked size, and the client, which holds the mode it asked for, hears the one that runs.
        RebuildAsk::Mode(mode, asked) if f.shared && mode != f.mode => {
            let framed = view_for(mode, asked, f.mode);
            if framed == view {
                return None;
            }
            RebuildPlan {
                build: Build::Encoder,
                rate_kbps: rate_at(f.mode, framed, f),
                trace: Some("resize"),
                epilogue: Epilogue {
                    tell: Some(Tell::Correction),
                    ..PIPELINE
                },
                rollback: Rollback {
                    tell: Some(Tell::Mode),
                    ..KEPT
                },
                ..at_live
            }
        }
        RebuildAsk::Mode(mode, asked) if (mode, asked) == (f.mode, view) => return None,
        // The display stays: the encoder alone reopens on the live capture, framed anew.
        RebuildAsk::Mode(mode, asked) if mode == f.mode => RebuildPlan {
            build: Build::Encoder,
            rate_kbps: rate_at(mode, asked, f),
            trace: Some("resize"),
            epilogue: Epilogue {
                tell: Some(Tell::Mode),
                ..PIPELINE
            },
            rollback: Rollback {
                tell: Some(Tell::Mode),
                ..KEPT
            },
            ..at_live
        },
        RebuildAsk::Mode(mode, asked) => RebuildPlan {
            in_place: f.windows && idd_push,
            mode,
            rate_kbps: rate_at(mode, asked, f),
            trace: Some("resize"),
            stops_joining: alone_on_joined,
            epilogue: Epilogue {
                tell: Some(Tell::Mode),
                ..rebuilt
            },
            rollback: Rollback {
                tell: Some(Tell::Mode),
                ..KEPT
            },
            ..at_live
        },
        // The depth, HDR and chroma rule judges the mode the session runs (S2-5).
        RebuildAsk::Format(codec) => RebuildPlan {
            reason: SwapReason::Format,
            display: Some(DisplayChange::Format(codec)),
            trace: Some("format"),
            stops_joining: alone_on_joined,
            epilogue: Epilogue {
                tell: Some(Tell::Codec),
                screens: Screens::Rebase,
                ..rebuilt
            },
            rollback: Rollback {
                tell: Some(Tell::Codec),
                ..KEPT
            },
            ..at_live
        },
        // A shared capture has one format for every session on it.
        RebuildAsk::Content(_) if f.shared || f.joined => return None,
        // gamescope's capture node feeds one link, so the running capture stops before the
        // next one attaches: from there it is a capture loss on a display that is still up.
        RebuildAsk::Content(hdr) => RebuildPlan {
            reason: SwapReason::Format,
            build: Build::CaptureLoss,
            display: Some(DisplayChange::Content(hdr)),
            trace: Some("format"),
            epilogue: Epilogue {
                input: f.follows_session,
                portal_cursor: true,
                screens: Screens::Rebase,
                keyframe: true,
                ..rebuilt
            },
            rollback: ENDS,
            ..at_live
        },
        RebuildAsk::TopologyReassert(generation)
            if !idd_push || generation == f.seen_reassert_gen =>
        {
            return None
        }
        // A transient first-frame race must not end the session: bring-up's attempts (S5-5).
        RebuildAsk::TopologyReassert(_) => RebuildPlan {
            reason: SwapReason::TopologyReassert,
            build: Build::Retry(1..=8),
            in_place: f.windows,
            trace: Some("reassert-recover"),
            epilogue: rebuilt,
            rollback: ENDS,
            ..at_live
        },
        RebuildAsk::CaptureLost(_) => RebuildPlan {
            reason: SwapReason::CaptureLost,
            build: Build::CaptureLoss,
            epilogue: Epilogue {
                input: f.follows_session,
                portal_cursor: true,
                screens: Screens::RebaseIfMoved,
                keyframe: true,
                ..rebuilt
            },
            rollback: ENDS,
            ..at_live
        },
        RebuildAsk::SourceMode(mode) => RebuildPlan {
            reason: SwapReason::SourceMode,
            build: Build::Encoder,
            mode,
            rate_kbps: rate_at(mode, view, f),
            epilogue: Epilogue {
                tell: Some(Tell::Correction),
                ..PIPELINE
            },
            ..at_live
        },
        RebuildAsk::Bitrate(kbps) => RebuildPlan {
            reason: SwapReason::Bitrate,
            build: Build::Encoder,
            rate_kbps: kbps,
            epilogue: Epilogue {
                rate: Rate::Applied,
                ..PIPELINE
            },
            rollback: Rollback {
                tell: Some(Tell::Rate),
                ..KEPT
            },
            ..at_live
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vdisplay::ActiveKind;

    const LIVE: Mode = Mode {
        width: 1920,
        height: 1080,
        refresh_hz: 60,
    };
    const BIG: Mode = Mode {
        width: 3840,
        height: 2160,
        refresh_hz: 120,
    };

    fn facts() -> RebuildFacts {
        RebuildFacts {
            mode: LIVE,
            rate_kbps: 20_000,
            plan: SessionPlan::resolve(
                &crate::encode::resolve(),
                8,
                false,
                crate::encode::ChromaFormat::Yuv420,
                Codec::H265,
                false,
                false,
                1,
            ),
            compositor: Compositor::Kwin,
            bitrate_auto: false,
            pyrowave_max_kbps: None,
            seen_reassert_gen: 3,
            follows_session: true,
            windows: false,
            shared: false,
            joined: false,
        }
    }

    const SMALL: Mode = Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };

    fn shared(f: &mut RebuildFacts) {
        f.shared = true;
    }

    fn joined(f: &mut RebuildFacts) {
        f.joined = true;
    }

    /// The encoder alone on a display another session holds, framing another size; the
    /// client hears the mode that runs, as a correction.
    fn framed() -> Option<RebuildPlan> {
        Some(RebuildPlan {
            trace: Some("resize"),
            epilogue: Epilogue {
                tell: Some(Tell::Correction),
                ..REBUILT
            },
            rollback: Rollback {
                tell: Some(Tell::Mode),
                ..SURVIVES
            },
            ..at_live(SwapReason::ModeSwitch, Build::Encoder)?
        })
    }

    /// The view a mode ask encodes: its own where the plan builds the asked mode, else the
    /// asked size on the display that stays, unless that size frames nothing.
    #[test]
    fn a_display_that_stays_frames_the_asked_size() {
        let wide = Mode {
            width: 2560,
            height: 720,
            refresh_hz: 60,
        };
        let faster = Mode {
            refresh_hz: 120,
            ..LIVE
        };
        assert_eq!(view_for(BIG, Some(VIEW), BIG), Some(VIEW), "built as asked");
        assert_eq!(view_for(BIG, None, BIG), None);
        assert_eq!(
            view_for(SMALL, None, LIVE),
            Some((1280, 720)),
            "the asked size"
        );
        assert_eq!(view_for(SMALL, Some((640, 360)), LIVE), Some((640, 360)));
        assert_eq!(view_for(faster, None, LIVE), None, "a refresh alone");
        assert_eq!(view_for(BIG, None, LIVE), None, "larger on both axes");
        assert_eq!(
            view_for(wide, None, LIVE),
            Some((2560, 720)),
            "shorter on one"
        );
    }

    fn switch(to: Compositor) -> RebuildAsk {
        RebuildAsk::SessionSwitch(SessionSwitch {
            kind: ActiveKind::Gaming,
            compositor: to,
            env: None,
            launch: None,
        })
    }

    fn idd_push(f: &mut RebuildFacts) {
        f.plan.capture = CaptureBackend::IddPush;
        f.windows = true;
    }

    fn pyrowave(f: &mut RebuildFacts) {
        f.plan.codec = Codec::PyroWave;
        f.plan.pyrowave_bpp = 4.0;
    }

    /// A full build's epilogue, every column spelled out.
    const REBUILT: Epilogue = Epilogue {
        tell: None,
        input: false,
        portal_cursor: false,
        screens: Screens::Keep,
        rate: Rate::Built,
        keyframe: false,
        reassert_sync: false,
    };

    /// A failed build the session survives: the stall announced, nobody told.
    const SURVIVES: Rollback = Rollback {
        gap: true,
        tell: None,
        ends: false,
    };

    /// The live mode rebuilt in a full build, the base every row moves from.
    fn at_live(reason: SwapReason, build: Build) -> Option<RebuildPlan> {
        Some(RebuildPlan {
            reason,
            build,
            in_place: false,
            mode: LIVE,
            rate_kbps: 20_000,
            display: None,
            trace: None,
            stops_joining: false,
            epilogue: REBUILT,
            rollback: SURVIVES,
        })
    }

    /// A mode switch to [`BIG`] at `rate_kbps`.
    fn to_big(rate_kbps: u32, in_place: bool) -> Option<RebuildPlan> {
        Some(RebuildPlan {
            in_place,
            mode: BIG,
            rate_kbps,
            trace: Some("resize"),
            epilogue: Epilogue {
                tell: Some(Tell::Mode),
                reassert_sync: in_place,
                ..REBUILT
            },
            rollback: Rollback {
                tell: Some(Tell::Mode),
                ..SURVIVES
            },
            ..at_live(SwapReason::ModeSwitch, Build::Once)?
        })
    }

    /// A topology re-assert that ends the session when it cannot build.
    fn reassert() -> Option<RebuildPlan> {
        Some(RebuildPlan {
            in_place: true,
            trace: Some("reassert-recover"),
            epilogue: Epilogue {
                reassert_sync: true,
                ..REBUILT
            },
            rollback: Rollback {
                ends: true,
                ..SURVIVES
            },
            ..at_live(SwapReason::TopologyReassert, Build::Retry(1..=8))?
        })
    }

    /// The encoder reopened at the delivered [`BIG`] for `rate_kbps`.
    fn source_mode(rate_kbps: u32) -> Option<RebuildPlan> {
        Some(RebuildPlan {
            mode: BIG,
            rate_kbps,
            epilogue: Epilogue {
                tell: Some(Tell::Correction),
                ..REBUILT
            },
            ..at_live(SwapReason::SourceMode, Build::Encoder)?
        })
    }

    fn pyrowave_pin() -> u32 {
        let mut f = facts();
        pyrowave(&mut f);
        pyrowave_mode_kbps(None, &BIG, &f.plan, None)
    }

    /// The view a client shows [`BIG`] at.
    const VIEW: (u32, u32) = (1920, 1080);

    fn viewed(f: &mut RebuildFacts) {
        f.plan.reframe_to = Some((punktfunk_core::video_fit::VideoFit::Fit, VIEW));
    }

    /// The encoder alone reopened on the live display for a new view, at `rate_kbps`.
    fn new_view(rate_kbps: u32) -> Option<RebuildPlan> {
        Some(RebuildPlan {
            rate_kbps,
            trace: Some("resize"),
            epilogue: Epilogue {
                tell: Some(Tell::Mode),
                ..REBUILT
            },
            rollback: Rollback {
                tell: Some(Tell::Mode),
                ..SURVIVES
            },
            ..at_live(SwapReason::ModeSwitch, Build::Encoder)?
        })
    }

    struct Row {
        name: &'static str,
        ask: fn() -> RebuildAsk,
        facts: fn(&mut RebuildFacts),
        want: fn() -> Option<RebuildPlan>,
    }

    const ROWS: &[Row] = &[
        Row {
            name: "session switch: a new display, bring-up's attempts; input follows on commit \
                   (S5-3), the further screens restart on it (S8-3)",
            ask: || switch(Compositor::Gamescope),
            facts: |_| {},
            want: || {
                Some(RebuildPlan {
                    display: Some(DisplayChange::Switch(Compositor::Gamescope)),
                    epilogue: Epilogue {
                        input: true,
                        portal_cursor: true,
                        screens: Screens::Rebase,
                        ..REBUILT
                    },
                    ..at_live(SwapReason::SessionSwitch, Build::Retry(1..=8))?
                })
            },
        },
        Row {
            name: "session switch to the compositor that runs: nothing",
            ask: || switch(Compositor::Kwin),
            facts: |_| {},
            want: || None,
        },
        Row {
            name: "mode switch: one build at the new mode, the client hears it; a failed one \
                   tells the mode kept (44692a0dc)",
            ask: || RebuildAsk::Mode(BIG, None),
            facts: |_| {},
            want: || to_big(20_000, false),
        },
        Row {
            name: "mode switch to the live mode: nothing (S5-6)",
            ask: || RebuildAsk::Mode(LIVE, None),
            facts: |_| {},
            want: || None,
        },
        Row {
            name: "the live mode and view asked again: nothing",
            ask: || RebuildAsk::Mode(LIVE, Some(VIEW)),
            facts: viewed,
            want: || None,
        },
        Row {
            name: "a view at the live mode: the encoder alone, framed for it; the client hears \
                   the mode",
            ask: || RebuildAsk::Mode(LIVE, Some(VIEW)),
            facts: |_| {},
            want: || new_view(20_000),
        },
        Row {
            name: "a view cleared at the live mode: the encoder alone, at the mode",
            ask: || RebuildAsk::Mode(LIVE, None),
            facts: viewed,
            want: || new_view(20_000),
        },
        Row {
            name: "mode switch on Windows IDD-push: in place first, the generation read again",
            ask: || RebuildAsk::Mode(BIG, None),
            facts: idd_push,
            want: || to_big(20_000, true),
        },
        Row {
            name: "PyroWave mode switch: the pin re-resolved at the mode built (S5-8)",
            ask: || RebuildAsk::Mode(BIG, None),
            facts: pyrowave,
            want: || to_big(pyrowave_pin(), false),
        },
        Row {
            name: "PyroWave mode switch with a view: the pin priced at the view",
            ask: || RebuildAsk::Mode(BIG, Some(VIEW)),
            facts: pyrowave,
            want: || {
                let mut f = facts();
                pyrowave(&mut f);
                let at_view = Mode {
                    width: VIEW.0,
                    height: VIEW.1,
                    ..BIG
                };
                let pin = pyrowave_mode_kbps(None, &at_view, &f.plan, None);
                assert!(pin < pyrowave_pin(), "a view prices fewer pixels");
                to_big(pin, false)
            },
        },
        Row {
            name: "codec change: re-resolved at the live mode (S2-5), the display request \
                   follows (S5-4), the further screens restart (S8-3)",
            ask: || RebuildAsk::Format(Codec::H264),
            facts: |_| {},
            want: || {
                Some(RebuildPlan {
                    display: Some(DisplayChange::Format(Codec::H264)),
                    trace: Some("format"),
                    epilogue: Epilogue {
                        tell: Some(Tell::Codec),
                        screens: Screens::Rebase,
                        ..REBUILT
                    },
                    rollback: Rollback {
                        tell: Some(Tell::Codec),
                        ..SURVIVES
                    },
                    ..at_live(SwapReason::Format, Build::Once)?
                })
            },
        },
        Row {
            name: "content turned HDR: the capture re-attaches as after a loss, in the new \
                   format, and the further screens restart",
            ask: || RebuildAsk::Content(true),
            facts: |_| {},
            want: || {
                Some(RebuildPlan {
                    display: Some(DisplayChange::Content(true)),
                    trace: Some("format"),
                    epilogue: Epilogue {
                        input: true,
                        portal_cursor: true,
                        screens: Screens::Rebase,
                        keyframe: true,
                        ..REBUILT
                    },
                    rollback: Rollback {
                        ends: true,
                        ..SURVIVES
                    },
                    ..at_live(SwapReason::Format, Build::CaptureLoss)?
                })
            },
        },
        Row {
            name: "content turned SDR on a display another session holds: nothing",
            ask: || RebuildAsk::Content(false),
            facts: |f| f.shared = true,
            want: || None,
        },
        Row {
            name: "topology re-assert: in place, then bring-up's attempts (S5-5); the \
                   generation read again on commit (S5-9)",
            ask: || RebuildAsk::TopologyReassert(4),
            facts: idd_push,
            want: reassert,
        },
        Row {
            name: "codec change on IDD-push: the generation read again on commit (S5-9)",
            ask: || RebuildAsk::Format(Codec::H264),
            facts: idd_push,
            want: || {
                Some(RebuildPlan {
                    display: Some(DisplayChange::Format(Codec::H264)),
                    trace: Some("format"),
                    epilogue: Epilogue {
                        tell: Some(Tell::Codec),
                        screens: Screens::Rebase,
                        reassert_sync: true,
                        ..REBUILT
                    },
                    rollback: Rollback {
                        tell: Some(Tell::Codec),
                        ..SURVIVES
                    },
                    ..at_live(SwapReason::Format, Build::Once)?
                })
            },
        },
        Row {
            name: "bitrate on IDD-push: an encoder alone leaves the generation (S5-9)",
            ask: || RebuildAsk::Bitrate(8_000),
            facts: idd_push,
            want: || {
                Some(RebuildPlan {
                    rate_kbps: 8_000,
                    epilogue: Epilogue {
                        rate: Rate::Applied,
                        ..REBUILT
                    },
                    rollback: Rollback {
                        tell: Some(Tell::Rate),
                        ..SURVIVES
                    },
                    ..at_live(SwapReason::Bitrate, Build::Encoder)?
                })
            },
        },
        Row {
            name: "topology re-assert of a generation already followed: nothing",
            ask: || RebuildAsk::TopologyReassert(3),
            facts: idd_push,
            want: || None,
        },
        Row {
            name: "topology re-assert with no IDD-push display: nothing",
            ask: || RebuildAsk::TopologyReassert(4),
            facts: |_| {},
            want: || None,
        },
        Row {
            name: "capture lost: the loss loop at the live mode (S6-4); input and the screens \
                   follow where it lands (S5-3); a failed one announces its stall (S5-2)",
            ask: || RebuildAsk::CaptureLost(None),
            facts: |_| {},
            want: || {
                Some(RebuildPlan {
                    epilogue: Epilogue {
                        input: true,
                        portal_cursor: true,
                        screens: Screens::RebaseIfMoved,
                        keyframe: true,
                        ..REBUILT
                    },
                    rollback: Rollback {
                        ends: true,
                        ..SURVIVES
                    },
                    ..at_live(SwapReason::CaptureLost, Build::CaptureLoss)?
                })
            },
        },
        Row {
            name: "capture lost under a compositor pin: input stays the operator's",
            ask: || RebuildAsk::CaptureLost(None),
            facts: |f| f.follows_session = false,
            want: || {
                Some(RebuildPlan {
                    epilogue: Epilogue {
                        portal_cursor: true,
                        screens: Screens::RebaseIfMoved,
                        keyframe: true,
                        ..REBUILT
                    },
                    rollback: Rollback {
                        ends: true,
                        ..SURVIVES
                    },
                    ..at_live(SwapReason::CaptureLost, Build::CaptureLoss)?
                })
            },
        },
        Row {
            name: "source mode: the encoder at the delivered mode, told as a correction; the \
                   session's encoder resolution (025b14da5)",
            ask: || RebuildAsk::SourceMode(BIG),
            facts: |_| {},
            want: || source_mode(20_000),
        },
        Row {
            name: "PyroWave source mode: the pin re-resolved at the delivered mode",
            ask: || RebuildAsk::SourceMode(BIG),
            facts: pyrowave,
            want: || source_mode(pyrowave_pin()),
        },
        Row {
            name: "shared display, a smaller mode asked: the encoder alone frames it at the live \
                   mode; the client hears that mode",
            ask: || RebuildAsk::Mode(SMALL, None),
            facts: shared,
            want: framed,
        },
        Row {
            name: "shared display, a larger mode asked: nothing to frame",
            ask: || RebuildAsk::Mode(BIG, None),
            facts: shared,
            want: || None,
        },
        Row {
            name: "shared display, a refresh alone asked: nothing",
            ask: || {
                let faster = Mode {
                    refresh_hz: 120,
                    ..LIVE
                };
                RebuildAsk::Mode(faster, None)
            },
            facts: shared,
            want: || None,
        },
        Row {
            name: "shared display, the size it already frames: nothing",
            ask: || RebuildAsk::Mode(SMALL, None),
            facts: |f| {
                shared(f);
                let fit = punktfunk_core::video_fit::VideoFit::Fit;
                f.plan.reframe_to = Some((fit, (1280, 720)));
            },
            want: || None,
        },
        Row {
            name:
                "a joiner on a shared display: framed as its owner is, never a display of its own",
            ask: || RebuildAsk::Mode(SMALL, None),
            facts: |f| {
                shared(f);
                joined(f);
            },
            want: framed,
        },
        Row {
            name: "a joiner alone on its display: a mode switch builds its own",
            ask: || RebuildAsk::Mode(BIG, None),
            facts: joined,
            want: || {
                Some(RebuildPlan {
                    stops_joining: true,
                    ..to_big(20_000, false)?
                })
            },
        },
        Row {
            name: "a joiner alone on its display: a codec change builds its own",
            ask: || RebuildAsk::Format(Codec::H264),
            facts: joined,
            want: || {
                Some(RebuildPlan {
                    display: Some(DisplayChange::Format(Codec::H264)),
                    trace: Some("format"),
                    stops_joining: true,
                    epilogue: Epilogue {
                        tell: Some(Tell::Codec),
                        screens: Screens::Rebase,
                        ..REBUILT
                    },
                    rollback: Rollback {
                        tell: Some(Tell::Codec),
                        ..SURVIVES
                    },
                    ..at_live(SwapReason::Format, Build::Once)?
                })
            },
        },
        Row {
            name: "bitrate: the encoder at the asked rate; a failed one tells the rate kept \
                   (S7-4)",
            ask: || RebuildAsk::Bitrate(8_000),
            facts: |_| {},
            want: || {
                Some(RebuildPlan {
                    rate_kbps: 8_000,
                    epilogue: Epilogue {
                        rate: Rate::Applied,
                        ..REBUILT
                    },
                    rollback: Rollback {
                        tell: Some(Tell::Rate),
                        ..SURVIVES
                    },
                    ..at_live(SwapReason::Bitrate, Build::Encoder)?
                })
            },
        },
    ];

    #[test]
    fn each_ask_plans_its_rebuild() {
        for row in ROWS {
            let mut f = facts();
            (row.facts)(&mut f);
            assert_eq!(plan(&(row.ask)(), &f), (row.want)(), "{}", row.name);
        }
    }
}
