//! Where pad input goes, over plain values. [`Gates`] holds what the client asked for, one
//! field per `Ctl`; [`Gates::input_route`] reads them as one [`InputRoute`]. [`route`] turns one
//! transition on a forwarded pad into the [`Action`]s the worker performs, in order.
//!
//! No SDL type: the worker maps SDL events to [`PadInput`] and re-reads the pad when an action
//! asks it to. The table test below pins every route.

use super::select_gesture::{select_chord, SelectChord, SelectGesture};
use punktfunk_core::client::Sc2Gate;
use punktfunk_core::input::gamepad as wire;
use std::time::{Duration, Instant};

/// The client's asks that decide where pad input goes. Each `Ctl` has its own caller, so the
/// fields are kept as sent and read together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Gates {
    /// Console navigation while no session is attached.
    pub(super) menu_mode: bool,
    /// The overlay owns the pads: neutral on the host, slots still open.
    pub(super) masked: bool,
    /// The ring or a console launch hold polls the pads: the forwarded ones, or the menu pads
    /// before a session attaches.
    pub(super) ring_nav: bool,
    /// Physical Guide and Quick Access reach the host; off, they stay with the local shell.
    pub(super) system_forward: bool,
    /// Something listens for a Select chord. Off, a chord would eat a press the game was owed.
    pub(super) chords_live: bool,
    /// Hold-Select is a synthetic host Guide ([`SelectGesture`]).
    pub(super) guide_gesture: bool,
}

impl Default for Gates {
    fn default() -> Gates {
        Gates {
            menu_mode: false,
            masked: false,
            ring_nav: false,
            system_forward: true,
            chords_live: false,
            guide_gesture: false,
        }
    }
}

/// Where pad input goes. Forwarded slots exist only with a session; menu pads only without.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InputRoute {
    /// No session, no menu, no ring: no pad is open.
    Closed,
    /// No session: every pad open and polled into menu events, for the console or for a ring
    /// or launch hold before attach.
    Menu,
    /// No session, menu pads open, the mask alone holds them: nothing moves.
    MenuHeld,
    /// Forwarded pads drive the host.
    Host,
    /// The overlay owns the pads: neutral on the host, slots open.
    Masked,
    /// The ring is up under the mask: forwarded pads polled into menu events.
    Ring,
    /// The ring without the mask: pads drive the host and the ring.
    HostAndRing,
}

/// The open pads menu navigation reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuReads {
    Nothing,
    /// What menu mode holds open.
    MenuPads,
    /// The forwarded slots, for the ring.
    Slots,
}

impl InputRoute {
    /// Menu pads are open (no session, and menu mode or the ring wants them).
    pub(super) fn opens_menu_pads(self) -> bool {
        matches!(self, InputRoute::Menu | InputRoute::MenuHeld)
    }

    pub(super) fn menu_reads(self) -> MenuReads {
        match self {
            InputRoute::Menu => MenuReads::MenuPads,
            InputRoute::Ring | InputRoute::HostAndRing => MenuReads::Slots,
            InputRoute::Closed | InputRoute::MenuHeld | InputRoute::Host | InputRoute::Masked => {
                MenuReads::Nothing
            }
        }
    }
}

impl Gates {
    pub(super) fn input_route(&self, session: bool) -> InputRoute {
        match (session, self.masked, self.ring_nav) {
            (true, false, false) => InputRoute::Host,
            (true, true, false) => InputRoute::Masked,
            (true, true, true) => InputRoute::Ring,
            (true, false, true) => InputRoute::HostAndRing,
            // A launch hold over a stream not yet attached: its B cancels the dial.
            (false, _, true) => InputRoute::Menu,
            (false, false, false) if self.menu_mode => InputRoute::Menu,
            (false, true, false) if self.menu_mode => InputRoute::MenuHeld,
            (false, ..) => InputRoute::Closed,
        }
    }

    /// The typed plane's mask, system-button routing and chord, for a raw Steam Controller.
    pub(super) fn sc2(&self) -> Sc2Gate {
        Sc2Gate {
            masked: self.masked,
            system_local: !self.system_forward,
            chords: self.chords_live,
        }
    }
}

/// A pad opened a moment ago reads every button up until its first input report. 50 ms
/// covers a 60 Hz report period three times over.
const MENU_SETTLE: Duration = Duration::from_millis(50);

/// Menu navigation adopts what is held, instead of firing it, for [`MENU_SETTLE`] after a
/// forwarded slot or a menu pad opens. A button held across attach, or while a pad is plugged in
/// under the launcher, would otherwise read up, then down.
#[derive(Default)]
pub(super) struct MenuSettle {
    until: Option<Instant>,
}

impl MenuSettle {
    pub(super) fn start(&mut self, now: Instant) {
        self.until = Some(now + MENU_SETTLE);
    }

    /// True while a menu poll must adopt held state rather than fire it.
    pub(super) fn adopting(&mut self, now: Instant) -> bool {
        let adopting = self.until.is_some_and(|t| now < t);
        if !adopting {
            self.until = None;
        }
        adopting
    }
}

/// One transition on a forwarded pad, with SDL's button already a wire bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PadInput {
    Button {
        bit: u32,
        down: bool,
    },
    /// A Steam pad's trackpad click: a touch surface, never the button plane.
    Click {
        surface: u8,
        down: bool,
    },
    /// A stick, trigger, touch or motion sample.
    Analog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    /// A button transition on the wire.
    Wire { bit: u32, down: bool },
    /// The trackpad click on its surface.
    Click { surface: u8, down: bool },
    /// The analog sample on the wire.
    Forward,
    /// Zero what the host holds for this pad.
    Flush,
    /// The local Guide lifted: re-read the pad into [`PadRoute::adopt`].
    Readopt,
    /// A Select chord for the client.
    Chord(SelectChord),
    /// A press took the host path: check the escape chord.
    Escape,
    /// A release took the host path: let go of a broken escape hold.
    Rearm,
}

/// One forwarded slot's routing memory.
#[derive(Default)]
pub(super) struct PadRoute {
    /// Wire bits down on this pad, a withheld chord press included.
    held: Vec<u32>,
    /// The button a Ring chord withheld: its release does not go out either.
    swallow: Option<u32>,
    /// Guide is down and stays local. It is the local shell's chord key (Steam: Guide+A opens
    /// the QAM), so the rest of this pad stays off the wire until it lifts.
    guide_local: bool,
    pub(super) gesture: SelectGesture,
}

impl PadRoute {
    /// Every press the host holds for this pad, as releases: the gesture's synthetic Guide or
    /// owed tap first (neither is in `held`), then each held button. A withheld chord press is
    /// forgotten too: under the mask its own release never arrives, so it would eat the next.
    pub(super) fn release(&mut self) -> Vec<(u32, bool)> {
        let mut out = Vec::new();
        self.gesture.flush(&mut out);
        self.swallow = None;
        out.extend(self.held.drain(..).map(|b| (b, false)));
        out
    }

    /// The pad is ours again. What is `down` now (wire bits, trackpad clicks left out) is held
    /// without a wire press: an A that picked a QAM row must not fire in the game. False while
    /// a local Guide still holds the pad; true when the caller re-sends the sticks.
    pub(super) fn adopt(
        &mut self,
        down: impl IntoIterator<Item = u32>,
        guide_down: bool,
        system_forward: bool,
    ) -> bool {
        self.held.clear();
        self.guide_local = !system_forward && guide_down;
        if self.guide_local {
            return false;
        }
        let ours = |b: &u32| system_forward || !is_system(*b);
        self.held.extend(down.into_iter().filter(ours));
        true
    }
}

fn is_system(bit: u32) -> bool {
    matches!(bit, wire::BTN_GUIDE | wire::BTN_MISC1)
}

/// Where one transition on a forwarded pad goes. Under the mask nothing reaches the host, but a
/// press still checks the escape chord: it is the pad's way back in and out. A local Guide holds
/// the rest of its pad until it lifts. A press meets the Select chord, then the guide gesture,
/// then the wire.
pub(super) fn route(
    input: PadInput,
    gates: &Gates,
    pad: &mut PadRoute,
    now: Instant,
) -> Vec<Action> {
    let guide = matches!(
        input,
        PadInput::Button {
            bit: wire::BTN_GUIDE,
            ..
        }
    );
    if gates.masked {
        return match input {
            PadInput::Button { down: true, .. } => vec![Action::Escape],
            _ => Vec::new(),
        };
    }
    if pad.guide_local && !guide {
        return Vec::new();
    }
    match input {
        PadInput::Analog => vec![Action::Forward],
        PadInput::Click { surface, down } => vec![Action::Click { surface, down }],
        PadInput::Button { bit, down: true } => press(bit, gates, pad, now),
        PadInput::Button { bit, down: false } => release(bit, gates, pad, now),
    }
}

fn press(bit: u32, gates: &Gates, pad: &mut PadRoute, now: Instant) -> Vec<Action> {
    if !gates.system_forward && is_system(bit) {
        // Neutral for the hold, as under the mask: what is pressed with Guide is the shell's
        // chord, and one already held must not stay down host-side.
        if bit == wire::BTN_GUIDE {
            pad.guide_local = true;
            return vec![Action::Flush];
        }
        return Vec::new();
    }
    let mut out = Vec::new();
    let chord = gates
        .chords_live
        .then(|| select_chord(&pad.held, bit, pad.gesture.is_guide(now)))
        .flatten();
    if let Some(chord) = chord {
        out.push(Action::Chord(chord));
        // A is the ring's own confirm, so the host never sees it. A pending Select goes with
        // it; one already on the wire is lifted by the ring's mask flush. Stats changes
        // nothing on the host, so that press carries on to the game.
        if chord == SelectChord::Ring {
            pad.gesture.swallow_for_ring();
            pad.swallow = Some(bit);
            pad.held.push(bit);
            return out;
        }
    }
    let mut due = Vec::new();
    let held_back = if !gates.guide_gesture {
        false
    } else if bit == wire::BTN_BACK {
        let alone = pad.held.is_empty();
        pad.gesture.on_select_down(now, alone, &mut due)
    } else {
        pad.gesture.on_other_down(bit, now, &mut due);
        false
    };
    out.extend(
        due.into_iter()
            .map(|(bit, down)| Action::Wire { bit, down }),
    );
    // Chord bookkeeping sees the physical press even when the gesture holds it back.
    pad.held.push(bit);
    if !held_back {
        out.push(Action::Wire { bit, down: true });
    }
    out.push(Action::Escape);
    out
}

fn release(bit: u32, gates: &Gates, pad: &mut PadRoute, now: Instant) -> Vec<Action> {
    // The host never saw this Guide go down: its lift only hands the pad back.
    if bit == wire::BTN_GUIDE && pad.guide_local {
        return vec![Action::Readopt];
    }
    if !gates.system_forward && is_system(bit) {
        return Vec::new();
    }
    pad.held.retain(|&b| b != bit);
    if pad.swallow == Some(bit) {
        pad.swallow = None;
        return Vec::new();
    }
    let mut due = Vec::new();
    let owned =
        gates.guide_gesture && bit == wire::BTN_BACK && pad.gesture.on_select_up(now, &mut due);
    let mut out: Vec<Action> = due
        .into_iter()
        .map(|(bit, down)| Action::Wire { bit, down })
        .collect();
    if !owned {
        out.push(Action::Wire { bit, down: false });
    }
    out.push(Action::Rearm);
    out
}

#[cfg(test)]
mod tests {
    use super::super::select_gesture::{GUIDE_COMMIT, GUIDE_HOLD, TAP_PRESS};
    use super::*;
    use std::time::Duration;
    use wire::{
        BTN_A as A, BTN_B as B, BTN_BACK as SELECT, BTN_GUIDE as GUIDE, BTN_LB as LB,
        BTN_MISC1 as QAM, BTN_RB as RB, BTN_START as START, BTN_X as X, BTN_Y as Y,
    };
    use Action::{Escape, Forward, Readopt, Rearm};

    fn host() -> Gates {
        Gates::default()
    }
    fn masked() -> Gates {
        Gates {
            masked: true,
            ..host()
        }
    }
    fn local() -> Gates {
        Gates {
            system_forward: false,
            ..host()
        }
    }
    fn chords() -> Gates {
        Gates {
            chords_live: true,
            ..host()
        }
    }
    fn gesture() -> Gates {
        Gates {
            guide_gesture: true,
            ..host()
        }
    }

    fn down(bit: u32) -> PadInput {
        PadInput::Button { bit, down: true }
    }
    fn up(bit: u32) -> PadInput {
        PadInput::Button { bit, down: false }
    }
    fn click(down: bool) -> PadInput {
        PadInput::Click { surface: 1, down }
    }
    fn on(bit: u32) -> Action {
        Action::Wire { bit, down: true }
    }
    fn off(bit: u32) -> Action {
        Action::Wire { bit, down: false }
    }

    /// One step on one forwarded pad, as the worker takes it.
    enum Step {
        /// A `Ctl` moves the gates.
        Set(Gates),
        /// A transition, and what [`route`] makes of it.
        In(PadInput, Vec<Action>),
        /// The worker's flush (mask on, or [`Action::Flush`]): the button releases it sends.
        Release(Vec<(u32, bool)>),
        /// The worker re-reads the pad: bits down, Guide down, and whether sticks are re-sent.
        Adopt(Vec<u32>, bool, bool),
        /// Time passes and the gesture's poll runs.
        Wait(Duration, Vec<(u32, bool)>),
    }
    use Step::{Adopt, In, Release, Set, Wait};

    fn walk(name: &str, steps: Vec<Step>) {
        let mut now = Instant::now();
        let mut gates = host();
        let mut pad = PadRoute::default();
        for (i, step) in steps.into_iter().enumerate() {
            match step {
                Set(g) => gates = g,
                In(input, want) => {
                    let got = route(input, &gates, &mut pad, now);
                    assert_eq!(got, want, "{name}, step {i}: {input:?}");
                }
                Release(want) => assert_eq!(pad.release(), want, "{name}, step {i}: flush"),
                Adopt(down, guide, want) => {
                    let got = pad.adopt(down, guide, gates.system_forward);
                    assert_eq!(got, want, "{name}, step {i}: adopt");
                }
                Wait(d, want) => {
                    now += d;
                    let mut got = Vec::new();
                    pad.gesture.poll(now, &mut got);
                    assert_eq!(got, want, "{name}, step {i}: poll");
                }
            }
        }
    }

    /// Every route a forwarded pad's input takes, plus one row per fix that moved one.
    #[test]
    fn every_route_of_a_forwarded_pad() {
        let chord = Action::Chord;
        let rows: Vec<(&str, Vec<Step>)> =
            vec![
            (
                "a button goes out and checks the escape chord",
                vec![In(down(A), vec![on(A), Escape]), In(up(A), vec![off(A), Rearm])],
            ),
            (
                "a stick, touch or motion sample goes out",
                vec![In(PadInput::Analog, vec![Forward])],
            ),
            (
                "a Steam trackpad click is a touch surface",
                vec![
                    In(click(true), vec![Action::Click { surface: 1, down: true }]),
                    In(click(false), vec![Action::Click { surface: 1, down: false }]),
                ],
            ),
            (
                "forwarded Guide and Quick Access are buttons",
                vec![
                    In(down(GUIDE), vec![on(GUIDE), Escape]),
                    In(down(QAM), vec![on(QAM), Escape]),
                    In(up(QAM), vec![off(QAM), Rearm]),
                    In(up(GUIDE), vec![off(GUIDE), Rearm]),
                ],
            ),
            (
                "a local Quick Access stays with the shell",
                vec![Set(local()), In(down(QAM), vec![]), In(up(QAM), vec![])],
            ),
            (
                "the ring without the mask leaves the host its input",
                vec![
                    Set(Gates {
                        ring_nav: true,
                        ..host()
                    }),
                    In(down(A), vec![on(A), Escape]),
                    In(PadInput::Analog, vec![Forward]),
                ],
            ),
            (
                "under the ring the host sees nothing",
                vec![
                    Set(Gates {
                        ring_nav: true,
                        ..masked()
                    }),
                    In(down(A), vec![Escape]),
                    In(PadInput::Analog, vec![]),
                ],
            ),
            (
                "8fe3637a3: a released capture masks the pads, then adopts what is held",
                vec![
                    In(down(A), vec![on(A), Escape]),
                    Set(masked()),
                    Release(vec![(A, false)]),
                    In(up(A), vec![]),
                    In(down(GUIDE), vec![Escape]),
                    In(PadInput::Analog, vec![]),
                    In(click(true), vec![]),
                    Set(host()),
                    Adopt(vec![X], false, true),
                    In(up(X), vec![off(X), Rearm]),
                ],
            ),
            (
                "the escape chord pressed again under the mask it raised is checked",
                vec![
                    Set(masked()),
                    In(down(LB), vec![Escape]),
                    In(down(RB), vec![Escape]),
                    In(down(START), vec![Escape]),
                    In(down(SELECT), vec![Escape]),
                    In(up(SELECT), vec![]),
                ],
            ),
            (
                "bb43ccda7: the escape hold outlives the mask it raises",
                vec![
                    Set(gesture()),
                    In(down(LB), vec![on(LB), Escape]),
                    In(down(RB), vec![on(RB), Escape]),
                    In(down(START), vec![on(START), Escape]),
                    In(down(SELECT), vec![on(SELECT), Escape]),
                    Set(Gates {
                        masked: true,
                        ..gesture()
                    }),
                    Release(vec![(LB, false), (RB, false), (START, false), (SELECT, false)]),
                    // No `Rearm` from a masked release: the disconnect poll reads the pads.
                    In(up(SELECT), vec![]),
                ],
            ),
            (
                "b42b33132: a local Guide holds its pad off the wire until it lifts",
                vec![
                    Set(local()),
                    In(down(A), vec![on(A), Escape]),
                    In(down(GUIDE), vec![Action::Flush]),
                    Release(vec![(A, false)]),
                    In(up(A), vec![]),
                    In(down(X), vec![]),
                    In(PadInput::Analog, vec![]),
                    In(click(true), vec![]),
                    In(up(GUIDE), vec![Readopt]),
                    Adopt(vec![X, QAM], false, true),
                    Release(vec![(X, false)]),
                ],
            ),
            (
                "b42b33132: a mask lifting under a held local Guide leaves the pad with the shell",
                vec![
                    Set(local()),
                    In(down(GUIDE), vec![Action::Flush]),
                    Release(vec![]),
                    Adopt(vec![A], true, false),
                    In(PadInput::Analog, vec![]),
                    In(up(GUIDE), vec![Readopt]),
                    Adopt(vec![], false, true),
                    In(PadInput::Analog, vec![Forward]),
                ],
            ),
            (
                "51adfbb8e: Select then A opens the ring without the guide gesture",
                vec![
                    Set(chords()),
                    In(down(SELECT), vec![on(SELECT), Escape]),
                    In(down(A), vec![chord(SelectChord::Ring)]),
                    In(up(A), vec![]),
                    In(up(SELECT), vec![off(SELECT), Rearm]),
                ],
            ),
            (
                "51adfbb8e: the ring's flush lifts the Select and forgets the withheld A",
                vec![
                    Set(chords()),
                    In(down(SELECT), vec![on(SELECT), Escape]),
                    In(down(A), vec![chord(SelectChord::Ring)]),
                    Set(Gates {
                        masked: true,
                        ring_nav: true,
                        ..chords()
                    }),
                    Release(vec![(SELECT, false), (A, false)]),
                    In(up(A), vec![]),
                    In(up(SELECT), vec![]),
                    Set(chords()),
                    Adopt(vec![], false, true),
                    In(down(A), vec![on(A), Escape]),
                    In(up(A), vec![off(A), Rearm]),
                ],
            ),
            (
                "with the gesture, a pending Select goes with the Ring chord",
                vec![
                    Set(Gates {
                        guide_gesture: true,
                        ..chords()
                    }),
                    In(down(SELECT), vec![Escape]),
                    In(down(A), vec![chord(SelectChord::Ring)]),
                    In(up(A), vec![]),
                    In(up(SELECT), vec![Rearm]),
                ],
            ),
            (
                "Select then X steps the stats and still presses X",
                vec![
                    Set(chords()),
                    In(down(SELECT), vec![on(SELECT), Escape]),
                    In(down(X), vec![chord(SelectChord::Stats), on(X), Escape]),
                    In(up(X), vec![off(X), Rearm]),
                ],
            ),
            (
                "a05b711ac: with nothing listening, Select then A is the game's",
                vec![
                    In(down(SELECT), vec![on(SELECT), Escape]),
                    In(down(A), vec![on(A), Escape]),
                ],
            ),
            (
                "a Select tap goes out on release, its lift later",
                vec![
                    Set(gesture()),
                    In(down(SELECT), vec![Escape]),
                    Wait(TAP_PRESS, vec![]),
                    In(up(SELECT), vec![on(SELECT), Rearm]),
                    Wait(TAP_PRESS, vec![(SELECT, false)]),
                ],
            ),
            (
                "a held Select is the host's Guide, and A then belongs to it",
                vec![
                    Set(Gates {
                        guide_gesture: true,
                        ..chords()
                    }),
                    In(down(SELECT), vec![Escape]),
                    Wait(GUIDE_HOLD, vec![]),
                    In(down(A), vec![on(GUIDE), on(A), Escape]),
                    In(up(A), vec![off(A), Rearm]),
                    In(up(SELECT), vec![off(GUIDE), Rearm]),
                ],
            ),
            (
                "a long Select hold sends Guide once no exit chord can follow",
                vec![
                    Set(gesture()),
                    In(down(SELECT), vec![Escape]),
                    Wait(GUIDE_HOLD, vec![]),
                    Wait(GUIDE_COMMIT - GUIDE_HOLD, vec![(GUIDE, true)]),
                    In(up(SELECT), vec![off(GUIDE), Rearm]),
                ],
            ),
            (
                "a Select released past the threshold taps Guide",
                vec![
                    Set(gesture()),
                    In(down(SELECT), vec![Escape]),
                    Wait(GUIDE_HOLD, vec![]),
                    In(up(SELECT), vec![on(GUIDE), Rearm]),
                    Wait(TAP_PRESS, vec![(GUIDE, false)]),
                ],
            ),
            (
                "an exit-chord partner after the threshold keeps the Select real",
                vec![
                    Set(gesture()),
                    In(down(SELECT), vec![Escape]),
                    Wait(GUIDE_HOLD * 2, vec![]),
                    In(down(START), vec![on(SELECT), on(START), Escape]),
                    Wait(GUIDE_COMMIT, vec![]),
                    In(up(SELECT), vec![off(SELECT), Rearm]),
                ],
            ),
            (
                "a press while Select is pending makes the Select real first",
                vec![
                    Set(gesture()),
                    In(down(SELECT), vec![Escape]),
                    In(down(Y), vec![on(SELECT), on(Y), Escape]),
                    In(up(SELECT), vec![off(SELECT), Rearm]),
                ],
            ),
            (
                "a flush lifts the gesture's synthetic Guide",
                vec![
                    Set(gesture()),
                    In(down(SELECT), vec![Escape]),
                    Wait(GUIDE_COMMIT, vec![(GUIDE, true)]),
                    Set(Gates {
                        masked: true,
                        ..gesture()
                    }),
                    Release(vec![(GUIDE, false), (SELECT, false)]),
                    In(up(SELECT), vec![]),
                ],
            ),
        ];
        for (name, steps) in rows {
            walk(name, steps);
        }
    }

    /// The exit chord in every press order, at three paces, released either way round: Select
    /// reaches the host as one plain hold and Guide never does. The worker polls every 10 ms.
    #[test]
    fn the_exit_chord_never_sends_guide() {
        fn orders(rest: &[u32]) -> Vec<Vec<u32>> {
            if rest.is_empty() {
                return vec![Vec::new()];
            }
            let mut out = Vec::new();
            for (i, &b) in rest.iter().enumerate() {
                let mut others = rest.to_vec();
                others.remove(i);
                for mut tail in orders(&others) {
                    tail.insert(0, b);
                    out.push(tail);
                }
            }
            out
        }
        let gates = Gates {
            guide_gesture: true,
            ..chords()
        };
        let tick = Duration::from_millis(10);
        for gap in [0, 150, 600].map(Duration::from_millis) {
            for order in orders(&[SELECT, START, LB, RB]) {
                let mut reversed = order.clone();
                reversed.reverse();
                for lift in [order.clone(), reversed] {
                    let t0 = Instant::now();
                    let mut steps: Vec<(Instant, PadInput)> = Vec::new();
                    for (i, &b) in order.iter().enumerate() {
                        steps.push((t0 + gap * i as u32, down(b)));
                    }
                    let held = t0 + gap * 3 + Duration::from_millis(1600);
                    for (i, &b) in lift.iter().enumerate() {
                        steps.push((held + gap * i as u32, up(b)));
                    }
                    let (mut pad, mut sent) = (PadRoute::default(), Vec::new());
                    let mut now = t0;
                    let end = held + gap * 3 + Duration::from_millis(200);
                    let mut next = steps.iter().peekable();
                    while now <= end {
                        while let Some(&&(at, input)) = next.peek() {
                            if at > now {
                                break;
                            }
                            next.next();
                            for a in route(input, &gates, &mut pad, now) {
                                if let Action::Wire { bit, down } = a {
                                    sent.push((bit, down));
                                }
                            }
                        }
                        pad.gesture.poll(now, &mut sent);
                        now += tick;
                    }
                    let at = format!("{order:?} lifted {lift:?}, {gap:?} apart: {sent:?}");
                    assert!(sent.iter().all(|&(b, _)| b != GUIDE), "Guide: {at}");
                    let select: Vec<_> = sent.iter().filter(|&&(b, _)| b == SELECT).collect();
                    assert_eq!(select, [&(SELECT, true), &(SELECT, false)], "{at}");
                }
            }
        }
    }

    /// Every combination of the asks: the route, what menu navigation reads, and whether a
    /// forwarded pad's press reaches the host.
    #[test]
    fn the_asks_read_as_one_route() {
        use InputRoute::*;
        use MenuReads::*;
        // (session, menu_mode, masked, ring_nav)
        let rows = [
            ((false, false, false, false), Closed, Nothing),
            ((false, false, false, true), Menu, MenuPads),
            ((false, false, true, false), Closed, Nothing),
            // A launch hold before attach reads the menu pads, with or without menu mode.
            ((false, false, true, true), Menu, MenuPads),
            ((false, true, false, false), Menu, MenuPads),
            // The mask alone, with no session, moves nothing.
            ((false, true, true, false), MenuHeld, Nothing),
            ((false, true, false, true), Menu, MenuPads),
            ((false, true, true, true), Menu, MenuPads),
            ((true, false, false, false), Host, Nothing),
            ((true, true, false, false), Host, Nothing),
            ((true, false, true, false), Masked, Nothing),
            ((true, true, true, false), Masked, Nothing),
            ((true, false, true, true), Ring, Slots),
            ((true, true, true, true), Ring, Slots),
            ((true, false, false, true), HostAndRing, Slots),
            ((true, true, false, true), HostAndRing, Slots),
        ];
        for ((session, menu_mode, masked, ring_nav), want, reads) in rows {
            let gates = Gates {
                menu_mode,
                masked,
                ring_nav,
                ..host()
            };
            let at = (session, menu_mode, masked, ring_nav);
            assert_eq!(gates.input_route(session), want, "{at:?}");
            assert_eq!(want.menu_reads(), reads, "{at:?}");
            let opens = !session && (menu_mode || ring_nav);
            assert_eq!(want.opens_menu_pads(), opens, "{at:?}");
            if session {
                let pad = &mut PadRoute::default();
                let sent = route(down(A), &gates, pad, Instant::now()).contains(&on(A));
                assert_eq!(sent, matches!(want, Host | HostAndRing), "{at:?}");
            }
        }
    }

    /// A B held through a launch hold and across attach never fires Back, so it cannot
    /// reveal the stream; the next real press does. The new slots read B up until they report.
    #[test]
    fn a_button_held_across_attach_is_adopted() {
        use crate::menu_nav::{MenuEvent, MenuNav, MenuSample};
        let b = |down| MenuSample {
            buttons: [false, down, false, false, false, false],
            ..MenuSample::default()
        };
        // What `menu_poll` does with one merged sample.
        fn poll(
            nav: &mut MenuNav,
            settle: &mut MenuSettle,
            s: &MenuSample,
            now: Instant,
        ) -> Vec<MenuEvent> {
            if settle.adopting(now) {
                nav.reset();
            }
            let mut out = Vec::new();
            nav.poll(s, now, &mut out);
            out
        }
        let (mut nav, mut settle) = (MenuNav::new(), MenuSettle::default());
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        // The hold reads the menu pads: B pressed then is a Back, for the cancel.
        assert!(poll(&mut nav, &mut settle, &b(false), ms(0)).is_empty());
        assert_eq!(
            poll(&mut nav, &mut settle, &b(true), ms(10)),
            [MenuEvent::Back]
        );
        // Attach with B still down: the slots open, the first sample reads B up.
        settle.start(ms(20));
        for (at, down) in [(20, false), (30, true), (60, true), (90, true)] {
            let fired = poll(&mut nav, &mut settle, &b(down), ms(at));
            assert!(fired.is_empty(), "B fired at {at} ms");
        }
        assert!(poll(&mut nav, &mut settle, &b(false), ms(100)).is_empty());
        assert_eq!(
            poll(&mut nav, &mut settle, &b(true), ms(110)),
            [MenuEvent::Back]
        );
        // The forwarded slot under the hold's mask sends nothing; the hand-back adopts B.
        let gates = Gates {
            masked: true,
            ring_nav: true,
            ..host()
        };
        let mut pad = PadRoute::default();
        assert_eq!(route(down(B), &gates, &mut pad, ms(30)), [Escape]);
        assert!(pad.adopt(vec![B], false, true));
        assert_eq!(route(up(B), &host(), &mut pad, ms(120)), [off(B), Rearm]);
    }

    /// a05b711ac: a raw Steam Controller runs the mask, system buttons and chord itself.
    #[test]
    fn a_raw_steam_controller_reads_the_same_asks() {
        let none = Sc2Gate::default();
        assert_eq!(host().sc2(), none);
        assert_eq!(
            masked().sc2(),
            Sc2Gate {
                masked: true,
                ..none
            }
        );
        assert_eq!(
            local().sc2(),
            Sc2Gate {
                system_local: true,
                ..none
            }
        );
        assert_eq!(
            chords().sc2(),
            Sc2Gate {
                chords: true,
                ..none
            }
        );
    }
}
