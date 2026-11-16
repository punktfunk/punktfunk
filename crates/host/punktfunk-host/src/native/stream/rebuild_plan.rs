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
use crate::native::geometry::{Hold, Intent, Tenure};
use crate::session_plan::{CaptureBackend, SessionPlan};
use crate::vdisplay::Compositor;
use punktfunk_core::Mode;

/// Why the loop rebuilds, with what the ask carries.
pub(super) enum RebuildAsk {
    /// Gaming↔Desktop.
    SessionSwitch(SessionSwitch),
    /// The client's accepted mode and view.
    Mode(Intent),
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
    /// What the session runs: the client's last ask, or the mode the source delivered.
    pub(super) intent: Intent,
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
    pub(super) tenure: Tenure,
    /// Another session is on the display: its mode stays put.
    pub(super) hold: Hold,
}

/// How the replacement is built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::native) enum Build {
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
    /// What is built, and the session's intent after: the derivation runs at its refresh (S5-8).
    pub(super) intent: Intent,
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

/// The rebuild `ask` needs, or `None`: a switch to the compositor that runs, the live mode and
/// view asked again (S5-6), on a shared display a size it already frames, a new mode on
/// gamescope, a topology generation already followed or no IDD-push to follow. A joiner alone
/// on its display builds its own.
pub(super) fn plan(ask: &RebuildAsk, f: &RebuildFacts) -> Option<RebuildPlan> {
    let live = f.intent;
    // PyroWave pins each intent's picture at the session's bits per pixel, and an Automatic
    // session keeps its rate inside the new bounds. Another codec's rate stands.
    let priced = |intent: &Intent| match f.plan.codec {
        Codec::PyroWave => {
            let running = f.bitrate_auto.then_some(f.rate_kbps);
            pyrowave_mode_kbps(running, &intent.picture(), &f.plan, f.pyrowave_max_kbps)
        }
        _ => f.rate_kbps,
    };
    let idd_push = f.plan.capture == CaptureBackend::IddPush;
    let rebuilt = Epilogue {
        reassert_sync: idd_push,
        ..PIPELINE
    };
    let at_live = RebuildPlan {
        reason: SwapReason::ModeSwitch,
        build: Build::Once,
        in_place: false,
        intent: live,
        rate_kbps: f.rate_kbps,
        display: None,
        trace: None,
        stops_joining: false,
        epilogue: rebuilt,
        rollback: KEPT,
    };
    let stops_joining = f.tenure.stops_joining(f.hold);
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
        // Another session is on the display, so its mode stays: the encoder alone frames the
        // asked size, and the client, which holds the mode it asked for, hears the one that runs.
        RebuildAsk::Mode(ask) if f.hold != Hold::Alone && ask.mode() != live.mode() => {
            let framed = ask.on_display(live.mode());
            if framed.view() == live.view() {
                return None;
            }
            RebuildPlan {
                build: Build::Encoder,
                intent: framed,
                rate_kbps: priced(&framed),
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
        RebuildAsk::Mode(ask) if (ask.mode(), ask.view()) == (live.mode(), live.view()) => {
            return None
        }
        // The display stays: the encoder alone reopens on the live capture, framed anew.
        RebuildAsk::Mode(ask) if ask.mode() == live.mode() => RebuildPlan {
            build: Build::Encoder,
            intent: ask,
            rate_kbps: priced(&ask),
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
        // A new mode respawns gamescope's nested game. A session that followed a switch onto it
        // keeps its display, and the client hears the mode that runs.
        RebuildAsk::Mode(_) if f.compositor == Compositor::Gamescope => return None,
        RebuildAsk::Mode(ask) => RebuildPlan {
            in_place: f.windows && idd_push,
            intent: ask,
            rate_kbps: priced(&ask),
            trace: Some("resize"),
            stops_joining,
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
            stops_joining,
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
        RebuildAsk::Content(_) if !f.tenure.follows_content(f.hold) => return None,
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
            intent: live.delivered_at(mode),
            rate_kbps: priced(&live.delivered_at(mode)),
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
        refresh_mhz: 0,
    };
    const BIG: Mode = Mode {
        width: 3840,
        height: 2160,
        refresh_hz: 120,
        refresh_mhz: 0,
    };

    fn facts() -> RebuildFacts {
        RebuildFacts {
            intent: Intent::of(LIVE, None),
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
            tenure: Tenure::of(None, false),
            hold: Hold::Alone,
        }
    }

    const SMALL: Mode = Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
        refresh_mhz: 0,
    };

    /// Another session joined the session's display.
    fn shared(f: &mut RebuildFacts) {
        f.hold = Hold::Shared;
    }

    /// The session joined the display, alone on it.
    fn joined(f: &mut RebuildFacts) {
        let at = Some((punktfunk_core::video_fit::VideoFit::Fit, (1920, 1080)));
        f.tenure = Tenure::of(at, false);
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
            refresh_mhz: 0,
        };
        let faster = Mode {
            refresh_hz: 120,
            ..LIVE
        };
        let framed = |asked, view, built| ask(asked, view).on_display(built).view();
        assert_eq!(framed(BIG, Some(VIEW), BIG), Some(VIEW), "built as asked");
        assert_eq!(framed(BIG, None, BIG), None);
        assert_eq!(
            framed(SMALL, None, LIVE),
            Some((1280, 720)),
            "the asked size"
        );
        assert_eq!(framed(SMALL, Some((640, 360)), LIVE), Some((640, 360)));
        assert_eq!(framed(faster, None, LIVE), None, "a refresh alone");
        assert_eq!(framed(BIG, None, LIVE), None, "larger on both axes");
        assert_eq!(
            framed(wide, None, LIVE),
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
            intent: Intent::of(LIVE, None),
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
            intent: Intent::of(BIG, None),
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
            intent: Intent::of(BIG, None),
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
        pyrowave_mode_kbps(None, &Picture::of(BIG), &f.plan, None)
    }

    /// The view a client shows [`BIG`] at.
    const VIEW: (u32, u32) = (1920, 1080);

    fn viewed(f: &mut RebuildFacts) {
        f.intent = ask(LIVE, Some(VIEW));
    }

    use crate::native::geometry::tests::asked as ask;
    use crate::native::geometry::Picture;

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
            ask: || RebuildAsk::Mode(ask(BIG, None)),
            facts: |_| {},
            want: || to_big(20_000, false),
        },
        Row {
            name: "mode switch to the live mode: nothing (S5-6)",
            ask: || RebuildAsk::Mode(ask(LIVE, None)),
            facts: |_| {},
            want: || None,
        },
        Row {
            name: "the live mode and view asked again: nothing",
            ask: || RebuildAsk::Mode(ask(LIVE, Some(VIEW))),
            facts: viewed,
            want: || None,
        },
        Row {
            name: "a view at the live mode: the encoder alone, framed for it; the client hears \
                   the mode",
            ask: || RebuildAsk::Mode(ask(LIVE, Some(VIEW))),
            facts: |_| {},
            want: || new_view(20_000),
        },
        Row {
            name: "a view cleared at the live mode: the encoder alone, at the mode",
            ask: || RebuildAsk::Mode(ask(LIVE, None)),
            facts: viewed,
            want: || new_view(20_000),
        },
        Row {
            name: "mode switch on Windows IDD-push: in place first, the generation read again",
            ask: || RebuildAsk::Mode(ask(BIG, None)),
            facts: idd_push,
            want: || to_big(20_000, true),
        },
        Row {
            name: "PyroWave mode switch: the pin re-resolved at the mode built (S5-8)",
            ask: || RebuildAsk::Mode(ask(BIG, None)),
            facts: pyrowave,
            want: || to_big(pyrowave_pin(), false),
        },
        Row {
            name: "PyroWave mode switch with a view: the pin priced at the view",
            ask: || RebuildAsk::Mode(ask(BIG, Some(VIEW))),
            facts: pyrowave,
            want: || {
                let mut f = facts();
                pyrowave(&mut f);
                let at_view = Mode {
                    width: VIEW.0,
                    height: VIEW.1,
                    ..BIG
                };
                let pin = pyrowave_mode_kbps(None, &Picture::of(at_view), &f.plan, None);
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
            facts: shared,
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
            ask: || RebuildAsk::Mode(ask(SMALL, None)),
            facts: shared,
            want: framed,
        },
        Row {
            name: "shared display, a larger mode asked: nothing to frame",
            ask: || RebuildAsk::Mode(ask(BIG, None)),
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
                RebuildAsk::Mode(ask(faster, None))
            },
            facts: shared,
            want: || None,
        },
        Row {
            name: "shared display, the size it already frames: nothing",
            ask: || RebuildAsk::Mode(ask(SMALL, None)),
            facts: |f| {
                shared(f);
                f.intent = ask(LIVE, Some((1280, 720)));
            },
            want: || None,
        },
        Row {
            name:
                "a joiner on a shared display: framed as its owner is, never a display of its own",
            ask: || RebuildAsk::Mode(ask(SMALL, None)),
            facts: |f| {
                joined(f);
                f.hold = Hold::Held;
            },
            want: framed,
        },
        Row {
            name: "a joiner alone on its display: a mode switch builds its own",
            ask: || RebuildAsk::Mode(ask(BIG, None)),
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
            // The rows name the mode built; `geometry_rows` checks the view.
            let planned = plan(&(row.ask)(), &f).map(|p| RebuildPlan {
                intent: Intent::of(p.intent.mode(), None),
                ..p
            });
            assert_eq!(planned, (row.want)(), "{}", row.name);
        }
    }

    use crate::native::handshake::tests::mode;

    /// How a plan builds, the mode it builds, the view `prepare` frames the build at and the
    /// rate. `None`: nothing rebuilds.
    type Geometry = Option<(Build, Mode, Option<(u32, u32)>, u32)>;

    /// [`Geometry`] of the plan for `ask`.
    fn geometry(ask: &RebuildAsk, f: &RebuildFacts) -> Geometry {
        let p = plan(ask, f)?;
        Some((p.build, p.intent.mode(), p.intent.view(), p.rate_kbps))
    }

    /// [`pyrowave`] at 1.6 bits per pixel, the rows' quality everywhere.
    fn pyro(f: &mut RebuildFacts) {
        pyrowave(f);
        f.plan.pyrowave_bpp = 1.6;
    }

    /// The session frames [`LIVE`] at 1280×720.
    fn framed_at_720(f: &mut RebuildFacts) {
        f.intent = ask(LIVE, Some((1280, 720)));
    }

    /// One ask against the facts the loop runs: what its plan builds, at which mode, view and
    /// rate.
    struct GeometryRow {
        name: &'static str,
        ask: RebuildAsk,
        facts: &'static [fn(&mut RebuildFacts)],
        want: Geometry,
        verdict: crate::native::tests::Verdict,
    }

    #[rustfmt::skip]
    fn geometry_rows_table() -> Vec<GeometryRow> {
        use crate::native::tests::Verdict::Wanted;
        use Build::{Encoder, Once};
        let (uhd, hd120, small120) = (mode(3840, 2160, 60), mode(1920, 1080, 120), mode(1280, 720, 120));
        let row = |name, ask, facts: &'static [fn(&mut RebuildFacts)], want: Geometry, verdict| {
            GeometryRow { name, ask, facts, want, verdict }
        };
        fn auto(f: &mut RebuildFacts) {
            (f.bitrate_auto, f.rate_kbps) = (true, 150_000);
        }
        fn hot(f: &mut RebuildFacts) {
            (f.bitrate_auto, f.rate_kbps) = (true, 500_000);
        }
        fn gamescope(f: &mut RebuildFacts) {
            f.compositor = Compositor::Gamescope;
        }
        vec![
            row("a new mode: one build at it, no view", RebuildAsk::Mode(ask(BIG, None)), &[],
                Some((Once, BIG, None, 20_000)), Wanted),
            row("a new mode with a view: one build, framed at the view",
                RebuildAsk::Mode(ask(uhd, Some((1920, 1080)))), &[],
                Some((Once, uhd, Some((1920, 1080)), 20_000)), Wanted),
            row("PyroWave, a new mode: priced at the mode", RebuildAsk::Mode(ask(uhd, None)), &[pyro],
                Some((Once, uhd, None, 796_262)), Wanted),
            row("PyroWave, a new mode with a view: priced at the view",
                RebuildAsk::Mode(ask(uhd, Some((1920, 1080)))), &[pyro],
                Some((Once, uhd, Some((1920, 1080)), 199_065)), Wanted),
            row("PyroWave Automatic: the running rate held inside the view's bounds",
                RebuildAsk::Mode(ask(uhd, Some((1920, 1080)))), &[pyro, auto],
                Some((Once, uhd, Some((1920, 1080)), 150_000)), Wanted),
            row("PyroWave Automatic above the view's pin: the pin",
                RebuildAsk::Mode(ask(uhd, Some((1920, 1080)))), &[pyro, hot],
                Some((Once, uhd, Some((1920, 1080)), 199_065)), Wanted),
            row("a view at the live mode: the encoder alone, framed",
                RebuildAsk::Mode(ask(LIVE, Some((1280, 720)))), &[],
                Some((Encoder, LIVE, Some((1280, 720)), 20_000)), Wanted),
            row("PyroWave, a view at the live mode: priced at the view",
                RebuildAsk::Mode(ask(LIVE, Some((1280, 720)))), &[pyro],
                Some((Encoder, LIVE, Some((1280, 720)), 88_473)), Wanted),
            row("a rate pick that keeps the view: one build at the rate, framed as before",
                RebuildAsk::Mode(ask(hd120, Some((1280, 720)))), &[pyro, framed_at_720],
                Some((Once, hd120, Some((1280, 720)), 176_947)), Wanted),
            row("a rate pick with no view: one build at the rate, the view cleared",
                RebuildAsk::Mode(ask(hd120, None)), &[pyro, framed_at_720],
                Some((Once, hd120, None, 398_131)), Wanted),
            row("shared: a smaller size framed on the live mode, priced at it",
                RebuildAsk::Mode(ask(SMALL, None)), &[pyro, shared],
                Some((Encoder, LIVE, Some((1280, 720)), 88_473)), Wanted),
            row("shared: a smaller size at another rate framed at the live rate",
                RebuildAsk::Mode(ask(small120, None)), &[pyro, shared],
                Some((Encoder, LIVE, Some((1280, 720)), 88_473)), Wanted),
            row("shared, framed: the framed size at another rate changes nothing",
                RebuildAsk::Mode(ask(small120, None)), &[pyro, shared, framed_at_720], None, Wanted),
            row("shared, framed: a refresh at the told size with its view changes nothing",
                RebuildAsk::Mode(ask(hd120, Some((1280, 720)))), &[pyro, shared, framed_at_720],
                None, Wanted),
            row("shared: a larger size changes nothing", RebuildAsk::Mode(ask(BIG, None)),
                &[shared], None, Wanted),
            row("shared, framed: a larger size clears the framing to the whole display",
                RebuildAsk::Mode(ask(BIG, None)), &[pyro, shared, framed_at_720],
                Some((Encoder, LIVE, None, 199_065)), Wanted),
            row("a joiner alone: a new mode builds its own display", RebuildAsk::Mode(ask(BIG, None)),
                &[joined], Some((Once, BIG, None, 20_000)), Wanted),
            row("source mode: the delivered mode, the view kept, priced at it",
                RebuildAsk::SourceMode(BIG), &[pyro, framed_at_720],
                Some((Encoder, BIG, Some((1280, 720)), 176_947)), Wanted),
            row("codec: the live mode and view", RebuildAsk::Format(Codec::H264),
                &[framed_at_720], Some((Once, LIVE, Some((1280, 720)), 20_000)), Wanted),
            row("capture lost: the live mode and view", RebuildAsk::CaptureLost(None),
                &[pyro, framed_at_720], Some((Build::CaptureLoss, LIVE, Some((1280, 720)), 20_000)),
                Wanted),
            row("rate: the asked rate, the view kept", RebuildAsk::Bitrate(8_000),
                &[framed_at_720], Some((Encoder, LIVE, Some((1280, 720)), 8_000)), Wanted),
            row("after a switch into gamescope: a new mode builds nothing",
                RebuildAsk::Mode(ask(BIG, None)), &[gamescope], None, Wanted),
            row("after a switch into gamescope: a view at the live mode still frames the encoder",
                RebuildAsk::Mode(ask(LIVE, Some((1280, 720)))), &[gamescope],
                Some((Encoder, LIVE, Some((1280, 720)), 20_000)), Wanted),
        ]
    }

    /// The mode, view and rate each ask builds at.
    #[test]
    fn geometry_rows() {
        for r in geometry_rows_table() {
            let mut f = facts();
            for set in r.facts {
                set(&mut f);
            }
            assert_eq!(geometry(&r.ask, &f), r.want, "{} ({:?})", r.name, r.verdict);
        }
    }
}
