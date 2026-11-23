//! The IDD-push open as a plan: every step a session's open takes, in order, decided before
//! any of it runs.
//!
//! [`plan_open`] turns what the session asked ([`OpenAsk`]) and what the host knows before
//! the open ([`DisplayFacts`]) into [`OpenStep`]s. [`run_colour`] runs the display's steps
//! over `DisplayColour`, with their 250 ms settle loops and log lines; `idd/open.rs` runs
//! the rest (the broker, the cursor channel, the diagnostics) and builds the capturer.
//!
//! Pure but for the seam, so a test per session kind runs the plan over a scripted display
//! on every target.

// Off Windows only the tests read this module.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use pf_win_display::colour::DisplayColour;
use pf_win_display::snapshot::{CcdTargetKey, ColorMode};

use crate::windows::depth_pin::{hdr_at_open, pin_wcg, settle, Composed, PinAction};

/// What the session asked for.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OpenAsk {
    /// The handshake advertised HDR, not merely 10-bit.
    pub(crate) want_hdr: bool,
    pub(crate) pyrowave: bool,
    /// A cursor channel sender: the session opted into the driver's hardware cursor.
    pub(crate) cursor_channel: bool,
    /// A cursor-forward sender.
    pub(crate) cursor_forward: bool,
}

/// What the host knows before the open.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DisplayFacts {
    /// SDR wide colour is open to this session: it asked for 10-bit SDR, the build has it, and
    /// no session on this host found it refused.
    pub(crate) wide_colour: bool,
    /// `PUNKTFUNK_IDD_DIAG` is on.
    pub(crate) diag: bool,
    /// `PUNKTFUNK_IDD_FLOW` is on.
    pub(crate) flow: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenStep {
    /// Open at the display's actual mode where it differs from the negotiated one: a
    /// fullscreen game can hold the virtual display at another.
    FollowActualMode,
    /// An SDR session turns advanced colour off, settled, so its encoder never emits PQ.
    PinAdvancedColourOff,
    /// An HDR session turns advanced colour on, settled when the write took.
    EnableHdr,
    /// Read advanced colour back ([`hdr_at_open`]). A 10-bit session it refused is loud.
    ReadHdr,
    /// An HDR session that did not take HDR reads whether the desktop composes wide colour.
    ObserveWcg,
    /// An SDR session pins wide colour: on for 10-bit SDR where it is open, else off.
    PinWcg(bool),
    /// The WUDFHost duplication target for the sections, and the driver-death probe.
    OpenBroker,
    /// Create the cursor section and hand it to the driver.
    DeliverCursorChannel,
    /// The cursor render model and, under any declare, the GDI poller.
    OpenCursorModel,
    /// Raise a forward flag an earlier session left down. The first tick stands it down
    /// again while the secure desktop is up.
    ResetCursorForward,
    /// One line with the mode and depth the display settled at.
    Bound,
    Diagnostics(Diagnostics),
    /// Keep the display lit while the capturer lives: DWM composes nothing for a dark one.
    HoldDisplayWake,
}

/// Which disturbance evidence runs this session. Probes and an ETW session alter the path a
/// stall report describes, so the posture is logged once per session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Diagnostics {
    /// Micro-probes, DxgKrnl ETW and the access-unit dump.
    Full,
    /// DxgKrnl ETW alone, with its present-flow line.
    Flow,
    Off,
}

impl Diagnostics {
    pub(crate) fn probes(self) -> bool {
        self == Diagnostics::Full
    }

    pub(crate) fn etw(self) -> bool {
        self != Diagnostics::Off
    }
}

pub(crate) fn plan_open(ask: &OpenAsk, facts: DisplayFacts) -> Vec<OpenStep> {
    use OpenStep::*;
    let mut plan = vec![FollowActualMode];
    // FP16 scRGB composition in advanced colour, BGRA otherwise. An SDR session pins it off:
    // PyroWave's CSC reads 8-bit BGRA only, and H.26x would emit P010 + PQ.
    if ask.want_hdr {
        plan.extend([EnableHdr, ReadHdr, ObserveWcg]);
    } else {
        plan.extend([PinAdvancedColourOff, ReadHdr, PinWcg(facts.wide_colour)]);
    }
    plan.push(OpenBroker);
    if ask.cursor_channel {
        plan.push(DeliverCursorChannel);
    }
    plan.push(OpenCursorModel);
    if ask.cursor_forward {
        plan.push(ResetCursorForward);
    }
    plan.push(Bound);
    plan.push(Diagnostics(match (facts.diag, facts.flow) {
        (true, _) => self::Diagnostics::Full,
        (false, true) => self::Diagnostics::Flow,
        (false, false) => self::Diagnostics::Off,
    }));
    plan.push(HoldDisplayWake);
    plan
}

/// What the display's steps settled at.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OpenedDisplay {
    pub(crate) mode: (u32, u32),
    pub(crate) composed: Composed,
    /// What the wide-colour pin means for the refusal latch; the shell applies it.
    pub(crate) verdicts: Vec<PinAction>,
}

/// Run the plan's display steps, in order, over `colour`. The other steps are the shell's.
pub(crate) fn run_colour(
    colour: &dyn DisplayColour,
    ccd: CcdTargetKey,
    target_id: u32,
    negotiated: (u32, u32),
    ask: &OpenAsk,
    plan: &[OpenStep],
) -> OpenedDisplay {
    let mut opened = OpenedDisplay {
        mode: negotiated,
        composed: Composed {
            hdr: false,
            wcg: false,
        },
        verdicts: Vec::new(),
    };
    let mut set_hdr = false;
    for step in plan {
        match *step {
            OpenStep::FollowActualMode => {
                opened.mode = follow_actual_mode(colour, ccd, target_id, negotiated);
            }
            OpenStep::PinAdvancedColourOff => pin_advanced_colour_off(colour, ccd, target_id, ask),
            OpenStep::EnableHdr => set_hdr = enable_hdr(colour, ccd, target_id),
            OpenStep::ReadHdr => {
                opened.composed.hdr = read_hdr(colour, ccd, target_id, ask, set_hdr)
            }
            OpenStep::ObserveWcg => {
                opened.composed.wcg =
                    !opened.composed.hdr && colour.color_mode(ccd) == Some(ColorMode::Wcg);
            }
            OpenStep::PinWcg(want) => {
                let (got, verdict) = pin_wcg(colour, ccd, target_id, want);
                opened.composed.wcg = got;
                opened.verdicts.extend(verdict);
            }
            _ => {}
        }
    }
    opened
}

/// The driver's encoder must open for what DWM composes.
fn follow_actual_mode(
    colour: &dyn DisplayColour,
    ccd: CcdTargetKey,
    target_id: u32,
    (pw, ph): (u32, u32),
) -> (u32, u32) {
    let (w, h) = colour.active_resolution(ccd).unwrap_or((pw, ph));
    if (w, h) != (pw, ph) {
        tracing::info!(
            target_id,
            negotiated = format!("{pw}x{ph}"),
            actual = format!("{w}x{h}"),
            "IDD push: opening at the display's actual mode (differs from negotiated)"
        );
    }
    (w, h)
}

fn pin_advanced_colour_off(
    colour: &dyn DisplayColour,
    ccd: CcdTargetKey,
    target_id: u32,
    ask: &OpenAsk,
) {
    let _ = colour.set_advanced_color(ccd, false);
    let settle = settle(|| colour.advanced_color_enabled(ccd) == Some(false));
    if colour.advanced_color_enabled(ccd) == Some(true) {
        tracing::error!(
            target = target_id,
            pyrowave = ask.pyrowave,
            "IDD push: SDR session but advanced color (HDR) could not be turned off on the \
             virtual display (a physical display forcing HDR?) — PyroWave will likely fail \
             its first frame; H.26x would emit PQ the SDR-only client never asked for"
        );
    } else {
        tracing::info!(
            target = target_id,
            pyrowave = ask.pyrowave,
            settle_ms = settle.elapsed().as_millis() as u64,
            "IDD push: SDR-negotiated session — advanced color forced OFF (SDR/BGRA composition)"
        );
    }
}

/// Whether the write took. A settle that times out still counts it: the driver's retained
/// pool slot absorbs a lagging compose flip.
fn enable_hdr(colour: &dyn DisplayColour, ccd: CcdTargetKey, target_id: u32) -> bool {
    let set = colour.set_advanced_color(ccd, true);
    if set {
        let settle = settle(|| colour.advanced_color_enabled(ccd) == Some(true));
        tracing::debug!(
            target_id,
            settle_ms = settle.elapsed().as_millis() as u64,
            "IDD push: advanced-color (HDR) enable settle"
        );
    }
    set
}

/// A 10-bit session that did not take HDR encodes 8-bit BT.709 while the client was told HDR,
/// until the descriptor poller sees HDR. The raw read tells "it said no" from "no answer".
fn read_hdr(
    colour: &dyn DisplayColour,
    ccd: CcdTargetKey,
    target_id: u32,
    ask: &OpenAsk,
    set: bool,
) -> bool {
    let observed = colour.advanced_color_enabled(ccd);
    let hdr = hdr_at_open(ask.want_hdr, set, observed);
    if ask.want_hdr && !hdr {
        tracing::error!(
            target = target_id,
            want_hdr = true,
            set_advanced_color_returned = set,
            observed_hdr = ?observed,
            "IDD push: 10-bit HDR was negotiated but enabling advanced color on the \
             virtual display failed — encoding 8-bit SDR while the client was told HDR \
             (check the display driver / Windows HDR support on this box). \
             observed_hdr=Some(false) ⇒ the display reports advanced colour OFF after the \
             set; None ⇒ the CCD read itself failed"
        );
    }
    hdr
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::cursor_state::{Action, CursorModel, Event, Opening};
    use crate::windows::depth_pin::fake::{ColourCall, FakeColour, Write};
    use ColorMode::{Hdr, Sdr, Wcg};
    use ColourCall::{
        ActiveResolution as Mode, AdvancedColorEnabled as ReadAc, ColorMode as Read,
        SetAdvancedColor as SetAc, SetWcg,
    };
    use OpenStep::*;

    const CCD: CcdTargetKey = CcdTargetKey {
        adapter_luid: 7,
        target_id: 3,
    };
    const NEGOTIATED: (u32, u32) = (1920, 1080);
    const SDR: OpenAsk = OpenAsk {
        want_hdr: false,
        pyrowave: false,
        cursor_channel: true,
        cursor_forward: true,
    };
    const HDR: OpenAsk = OpenAsk {
        want_hdr: true,
        ..SDR
    };
    const FACTS: DisplayFacts = DisplayFacts {
        wide_colour: false,
        diag: false,
        flow: false,
    };
    /// What every plan runs after the display's steps, with a cursor channel and a sender.
    const SESSION: [OpenStep; 7] = [
        OpenBroker,
        DeliverCursorChannel,
        OpenCursorModel,
        ResetCursorForward,
        Bound,
        Diagnostics(super::Diagnostics::Off),
        HoldDisplayWake,
    ];

    fn steps(display: &[OpenStep], session: &[OpenStep]) -> Vec<OpenStep> {
        [&[FollowActualMode], display, session].concat()
    }

    fn composed(hdr: bool, wcg: bool) -> Composed {
        Composed { hdr, wcg }
    }

    /// Plan `ask` and run its display steps over `fake`: the plan, what it settled at, and the
    /// calls with a settle loop's repeated reads counted once. The refusal warning's
    /// `wcg_supported` read happens only under a subscriber, so it is left out.
    fn open(
        fake: &FakeColour,
        ask: OpenAsk,
        facts: DisplayFacts,
    ) -> (Vec<OpenStep>, OpenedDisplay, Vec<ColourCall>) {
        let plan = plan_open(&ask, facts);
        let opened = run_colour(fake, CCD, 3, NEGOTIATED, &ask, &plan);
        let mut calls = fake.take_calls();
        calls.retain(|c| *c != ColourCall::WcgSupported);
        calls.dedup();
        (plan, opened, calls)
    }

    #[test]
    fn an_sdr_open_pins_advanced_and_wide_colour_off() {
        let fake = FakeColour::new(Hdr);
        let (plan, opened, calls) = open(&fake, SDR, FACTS);
        assert_eq!(
            plan,
            steps(&[PinAdvancedColourOff, ReadHdr, PinWcg(false)], &SESSION)
        );
        assert_eq!(calls, [Mode, SetAc(false), ReadAc, Read]);
        assert_eq!(opened.mode, NEGOTIATED);
        assert_eq!(opened.composed, composed(false, false));
        assert_eq!(opened.verdicts, []);
    }

    #[test]
    fn an_hdr_open_enables_and_settles() {
        let fake = FakeColour::new(Sdr);
        let (plan, opened, calls) = open(&fake, HDR, FACTS);
        assert_eq!(plan, steps(&[EnableHdr, ReadHdr, ObserveWcg], &SESSION));
        // HDR took, so wide colour is not read.
        assert_eq!(calls, [Mode, SetAc(true), ReadAc]);
        assert_eq!(opened.composed, composed(true, false));

        // A refused write is no settle; the session observes what the desktop composes.
        let fake = FakeColour::new(Wcg);
        fake.script(|s| s.writes.push_back(Write::Refused));
        let (_, opened, calls) = open(&fake, HDR, FACTS);
        assert_eq!(calls, [Mode, SetAc(true), ReadAc, Read]);
        assert_eq!(opened.composed, composed(false, true));

        // A write that lags its read still counts: the read says SDR, the open says HDR.
        let fake = FakeColour::new(Sdr);
        fake.script(|s| s.writes.push_back(Write::Ignored));
        let (_, opened, calls) = open(&fake, HDR, FACTS);
        assert_eq!(calls, [Mode, SetAc(true), ReadAc]);
        assert_eq!(opened.composed, composed(true, false));
    }

    #[test]
    fn a_ten_bit_sdr_open_takes_wide_colour_or_widens() {
        let open_to_wcg = DisplayFacts {
            wide_colour: true,
            ..FACTS
        };
        let display = [PinAdvancedColourOff, ReadHdr, PinWcg(true)];
        let fake = FakeColour::new(Sdr);
        let (plan, opened, calls) = open(&fake, SDR, open_to_wcg);
        assert_eq!(plan, steps(&display, &SESSION));
        assert_eq!(
            calls,
            [Mode, SetAc(false), ReadAc, Read, SetWcg(true), Read]
        );
        assert_eq!(opened.composed, composed(false, true));
        assert_eq!(opened.verdicts, [PinAction::ClearWcgRefused]);

        // Refused on a readable mode: the session widens its 8-bit desktop and latches.
        let fake = FakeColour::new(Sdr);
        fake.script(|s| s.writes.extend([Write::Takes, Write::Ignored]));
        let (_, opened, calls) = open(&fake, SDR, open_to_wcg);
        assert_eq!(
            calls,
            [Mode, SetAc(false), ReadAc, Read, SetWcg(true), Read]
        );
        assert_eq!(opened.composed, composed(false, false));
        assert_eq!(opened.verdicts, [PinAction::LatchWcgRefused]);

        // Wide colour already refused on this host, or an older build: pinned off.
        let fake = FakeColour::new(Wcg);
        fake.script(|s| s.writes.push_back(Write::Ignored));
        let (plan, opened, calls) = open(&fake, SDR, FACTS);
        assert_eq!(
            plan,
            steps(&[PinAdvancedColourOff, ReadHdr, PinWcg(false)], &SESSION)
        );
        assert_eq!(
            calls,
            [Mode, SetAc(false), ReadAc, Read, SetWcg(false), Read]
        );
        assert_eq!(opened.composed, composed(false, false));
        assert_eq!(opened.verdicts, []);
    }

    /// A wavelet session opens as its depth's H.26x one does; the pin that differs is the
    /// poll's, which holds even an HDR wavelet session's advanced colour (`depth_pin`).
    #[test]
    fn a_wavelet_open_plans_as_its_depth_does() {
        for ask in [SDR, HDR] {
            let wavelet = OpenAsk {
                pyrowave: true,
                ..ask
            };
            assert_eq!(plan_open(&wavelet, FACTS), plan_open(&ask, FACTS));
        }
    }

    #[test]
    fn the_cursor_steps_follow_the_senders() {
        let without = OpenAsk {
            cursor_channel: false,
            cursor_forward: false,
            ..SDR
        };
        let session = [
            OpenBroker,
            OpenCursorModel,
            Bound,
            Diagnostics(super::Diagnostics::Off),
            HoldDisplayWake,
        ];
        let display = [PinAdvancedColourOff, ReadHdr, PinWcg(false)];
        assert_eq!(plan_open(&without, FACTS), steps(&display, &session));
        assert_eq!(plan_open(&SDR, FACTS), steps(&display, &SESSION));
    }

    /// The open neither reads the secure desktop nor holds the declare down for it: the reset
    /// raises the flag, and the first tick's secure-desktop poll stands it down.
    #[test]
    fn the_secure_desktop_at_open_is_the_first_ticks() {
        let fake = FakeColour::new(Sdr);
        fake.script(|s| s.secure = true);
        let (plan, _, calls) = open(&fake, SDR, FACTS);
        assert!(plan.contains(&ResetCursorForward));
        assert!(!calls.contains(&ColourCall::SecureDesktop));

        let (mut model, reset) = CursorModel::open(Opening {
            excluded: false,
            channel: true,
            client_draws: true,
        });
        assert_eq!(reset, Some(true));
        assert_eq!(model.step(Event::Secure(true)), Action::StandDown);
    }

    #[test]
    fn the_open_follows_the_display_s_actual_mode() {
        let fake = FakeColour::new(Sdr);
        fake.script(|s| s.resolution = Some((2560, 1440)));
        assert_eq!(open(&fake, SDR, FACTS).1.mode, (2560, 1440));
        let fake = FakeColour::new(Sdr);
        assert_eq!(open(&fake, SDR, FACTS).1.mode, NEGOTIATED);
    }

    #[test]
    fn the_diagnostics_follow_the_knobs() {
        let posture = |diag, flow| {
            let facts = DisplayFacts {
                diag,
                flow,
                ..FACTS
            };
            let plan = plan_open(&SDR, facts);
            plan.iter()
                .find_map(|s| match s {
                    Diagnostics(d) => Some(*d),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(posture(true, false), super::Diagnostics::Full);
        assert_eq!(posture(true, true), super::Diagnostics::Full);
        assert_eq!(posture(false, true), super::Diagnostics::Flow);
        assert_eq!(posture(false, false), super::Diagnostics::Off);
        let full = super::Diagnostics::Full;
        assert!(full.probes() && full.etw());
        let flow = super::Diagnostics::Flow;
        assert!(!flow.probes() && flow.etw());
    }
}
