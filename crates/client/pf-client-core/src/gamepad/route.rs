//! Where pad input goes, over plain values. [`Gates`] holds what the client asked for, one
//! field per `Ctl`; [`Gates::input_route`] reads them as one [`InputRoute`]. [`route`] turns one
//! transition on a forwarded pad into the [`Action`]s the worker performs, in order.
//!
//! Each pad's shortcut [`Engine`] reads its buttons before the mask: only what the engine
//! forwards obeys the mask and a local Guide's hold; a fired shortcut and its lift do not.
//! A Guide or Quick Access the local shell keeps never reaches the engine.
//!
//! No SDL type: the worker maps SDL events to [`PadInput`] and re-reads the pad when an action
//! asks it to. The table test below pins every route.

use crate::overlay_actions::SlotId;
use crate::shortcuts::{self, Binding, Device, Engine, Event, Out, Phase, TAP_PRESS};
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
    /// A shortcut opens the ring on Select then A ([`ring_on_select_a`]).
    pub(super) ring_chord: bool,
}

impl Default for Gates {
    fn default() -> Gates {
        Gates {
            menu_mode: false,
            masked: false,
            ring_nav: false,
            system_forward: true,
            ring_chord: false,
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
            chords: self.ring_chord,
        }
    }
}

/// Whether a pad press of Select and A opens the ring: the one chord a raw Steam Controller's
/// own filter keeps off its plane.
pub(super) fn ring_on_select_a(bindings: &[Binding]) -> bool {
    let mut chord = vec![wire::BTN_BACK, wire::BTN_A];
    chord.sort_unstable();
    bindings.iter().any(|b| {
        b.action == shortcuts::Action::Ring
            && b.trigger.device() == Device::Pad
            && !b.trigger.hold()
            && b.trigger.ids() == chord
    })
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

/// A synthetic tap's wire events as `(bit, down, after)`: each press [`TAP_PRESS`] after
/// the one before, then every release together, last pressed first. Apart, because core
/// folds per-transition sends into one state, and because a chord's first button has to be
/// held before its second counts.
pub(super) fn tap_events(presses: &[u32]) -> Vec<(u32, bool, Duration)> {
    let downs = presses.iter().zip(0u32..);
    let mut events: Vec<_> = downs.map(|(&b, i)| (b, true, TAP_PRESS * i)).collect();
    let up = TAP_PRESS * presses.len() as u32;
    events.extend(presses.iter().rev().map(|&b| (b, false, up)));
    events
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

#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// A shortcut fired an action the client performs.
    Fire(shortcuts::Action, Phase),
}

/// One forwarded slot's routing memory.
pub(super) struct PadRoute {
    /// Wire bits the host may hold for this pad: forwarded presses, a shortcut's Guide held
    /// down, and what [`Self::adopt`] took.
    held: Vec<u32>,
    /// Guide is down and stays local. It is the local shell's chord key (Steam: Guide+A opens
    /// the QAM), so the rest of this pad stays off the wire until it lifts.
    guide_local: bool,
    shortcuts: Engine,
    /// What Quick Access presses on this pad's host device ([`wire::quick_access_presses`]).
    quick_access: &'static [u32],
    /// A shortcut's Guide or Quick Access presses still due, in send order.
    owed: Vec<(Instant, u32, bool)>,
}

impl PadRoute {
    pub(super) fn new(bindings: &[Binding], quick_access: &'static [u32]) -> PadRoute {
        PadRoute {
            held: Vec::new(),
            guide_local: false,
            shortcuts: Engine::new(bindings, Device::Pad),
            quick_access,
            owed: Vec::new(),
        }
    }

    /// Every press the host holds for this pad, as releases: a shortcut's owed releases, then
    /// each held button. The shortcuts keep their state: a hold outlives the mask.
    pub(super) fn release(&mut self) -> Vec<(u32, bool)> {
        let owed = self.owed.drain(..).filter(|&(_, _, down)| !down);
        let mut out: Vec<_> = owed.map(|(_, b, _)| (b, false)).collect();
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

    /// The clock: owed Guide presses that came due, then a hold that fired.
    pub(super) fn poll(&mut self, gates: &Gates, now: Instant) -> Vec<Action> {
        let mut acts = Vec::new();
        self.owed.retain(|&(due, bit, down)| {
            let keep = now < due;
            if !keep {
                acts.push(Action::Wire { bit, down });
            }
            keep
        });
        let outs = self.shortcuts.tick(now);
        acts.extend(self.perform(outs, gates, now));
        acts
    }

    /// The slot closes, or its bindings change: whatever the shortcuts hold comes up.
    pub(super) fn flush(&mut self, gates: &Gates, now: Instant) -> Vec<Action> {
        let outs = self.shortcuts.flush();
        self.perform(outs, gates, now)
    }

    /// The next stream's bindings, after a [`Self::flush`] of the old ones.
    pub(super) fn rebind(
        &mut self,
        bindings: &[Binding],
        gates: &Gates,
        now: Instant,
    ) -> Vec<Action> {
        let acts = self.flush(gates, now);
        self.shortcuts = Engine::new(bindings, Device::Pad);
        acts
    }

    fn perform(&mut self, outs: Vec<Out>, gates: &Gates, now: Instant) -> Vec<Action> {
        let mut acts = Vec::new();
        for out in outs {
            match out {
                Out::Forward(..) if gates.masked || self.guide_local => {}
                Out::Forward(bit, down) => {
                    self.held.retain(|&b| b != bit);
                    if down {
                        self.held.push(bit);
                    }
                    acts.push(Action::Wire { bit, down });
                }
                Out::Lift(bits) => {
                    for bit in bits {
                        if self.held.contains(&bit) {
                            self.held.retain(|&b| b != bit);
                            acts.push(Action::Wire { bit, down: false });
                        }
                    }
                }
                Out::Fire(shortcuts::Action::Slot(SlotId::Guide), phase) => {
                    self.system(&[wire::BTN_GUIDE], phase, now, &mut acts);
                }
                Out::Fire(shortcuts::Action::Slot(SlotId::Qam), phase) => {
                    let presses = self.quick_access;
                    self.system(presses, phase, now, &mut acts);
                }
                Out::Fire(action, phase) => acts.push(Action::Fire(action, phase)),
            }
        }
        acts
    }

    /// A shortcut's Guide or Quick Access on this pad's wire: a tap, or the presses held down
    /// until its `Up`. After whatever this pad still owes, so a quick Up never overtakes a press.
    fn system(&mut self, presses: &[u32], phase: Phase, now: Instant, acts: &mut Vec<Action>) {
        let start = self
            .owed
            .iter()
            .map(|o| o.0)
            .max()
            .map_or(now, |t| t.max(now));
        let events: Vec<_> = match phase {
            Phase::Once | Phase::Tap => tap_events(presses),
            Phase::Down => presses
                .iter()
                .zip(0u32..)
                .map(|(&b, i)| (b, true, TAP_PRESS * i))
                .collect(),
            Phase::Up => presses
                .iter()
                .rev()
                .map(|&b| (b, false, Duration::ZERO))
                .collect(),
        };
        for (bit, down, after) in events {
            self.held.retain(|&b| b != bit);
            if phase == Phase::Down {
                self.held.push(bit);
            }
            match start + after {
                due if due <= now => acts.push(Action::Wire { bit, down }),
                due => self.owed.push((due, bit, down)),
            }
        }
    }
}

fn is_system(bit: u32) -> bool {
    matches!(bit, wire::BTN_GUIDE | wire::BTN_MISC1)
}

/// Where one transition on a forwarded pad goes. A Guide or Quick Access the local shell keeps
/// goes to the shell; every other button goes through the shortcuts, which forward what is the
/// game's. Under the mask or a local Guide nothing reaches the host but what a shortcut sends.
pub(super) fn route(
    input: PadInput,
    gates: &Gates,
    pad: &mut PadRoute,
    now: Instant,
) -> Vec<Action> {
    let off = gates.masked || pad.guide_local;
    match input {
        PadInput::Analog if !off => vec![Action::Forward],
        PadInput::Click { surface, down } if !off => vec![Action::Click { surface, down }],
        PadInput::Analog | PadInput::Click { .. } => Vec::new(),
        // The host never saw this Guide go down: its lift only hands the pad back.
        PadInput::Button {
            bit: wire::BTN_GUIDE,
            down: false,
        } if pad.guide_local && !gates.masked => vec![Action::Readopt],
        PadInput::Button { bit, down } if !gates.system_forward && is_system(bit) => {
            // Neutral for the hold, as under the mask: what is pressed with Guide is the
            // shell's chord, and one already held must not stay down host-side.
            if bit == wire::BTN_GUIDE && down && !gates.masked {
                pad.guide_local = true;
                return vec![Action::Flush];
            }
            Vec::new()
        }
        PadInput::Button { bit, down } => {
            let event = if down {
                Event::Press(bit)
            } else {
                Event::Release(bit)
            };
            let outs = pad.shortcuts.step(event, now);
            let tap = outs.contains(&Out::Forward(bit, true));
            let acts = pad.perform(outs, gates, now);
            // A release the engine kept was never sent, or was lifted when its chord fired.
            if !down && !tap {
                pad.held.retain(|&b| b != bit);
            }
            acts
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shortcuts::{defaults, Client, Trigger, HOLD, KEY_TAP};
    use wire::{
        BTN_A as A, BTN_B as B, BTN_BACK as SELECT, BTN_GUIDE as GUIDE, BTN_LB as LB,
        BTN_MISC1 as QAM, BTN_RB as RB, BTN_START as START, BTN_X as X, BTN_Y as Y,
    };
    use Action::{Forward, Readopt};

    /// The desktop's defaults: under Game Mode they hold Select for Guide.
    fn desktop() -> Vec<Binding> {
        defaults(Client::Desktop, false, "auto").bindings
    }
    fn deck() -> Vec<Binding> {
        defaults(Client::Desktop, true, "auto").bindings
    }
    /// A client with no ring to open.
    fn no_ring() -> Vec<Binding> {
        let mut b = desktop();
        b.retain(|b| b.action != shortcuts::Action::Ring);
        b
    }

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
    fn fire(slot: SlotId) -> Action {
        Action::Fire(shortcuts::Action::Slot(slot), Phase::Once)
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
        /// Time passes and the worker's poll runs.
        Wait(Duration, Vec<Action>),
    }
    use Step::{Adopt, In, Release, Set, Wait};

    fn walk(name: &str, bindings: Vec<Binding>, steps: Vec<Step>) {
        let mut now = Instant::now();
        let mut gates = host();
        let mut pad = PadRoute::new(&bindings, wire::quick_access_presses(false));
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
                    let got = pad.poll(&gates, now);
                    assert_eq!(got, want, "{name}, step {i}: poll");
                }
            }
        }
    }

    /// Every route a forwarded pad's input takes, plus one row per fix that moved one.
    #[test]
    fn every_route_of_a_forwarded_pad() {
        let guide_a = Binding {
            trigger: Trigger::Pad {
                buttons: vec![GUIDE, A],
                hold: false,
            },
            action: shortcuts::Action::Ring,
        };
        let rows: Vec<(&str, Vec<Binding>, Vec<Step>)> = vec![
            (
                "a button goes out",
                desktop(),
                vec![In(down(A), vec![on(A)]), In(up(A), vec![off(A)])],
            ),
            (
                "a stick, touch or motion sample goes out",
                desktop(),
                vec![In(PadInput::Analog, vec![Forward])],
            ),
            (
                "a Steam trackpad click is a touch surface",
                desktop(),
                vec![
                    In(
                        click(true),
                        vec![Action::Click {
                            surface: 1,
                            down: true,
                        }],
                    ),
                    In(
                        click(false),
                        vec![Action::Click {
                            surface: 1,
                            down: false,
                        }],
                    ),
                ],
            ),
            (
                "forwarded Guide and Quick Access are buttons",
                desktop(),
                vec![
                    In(down(GUIDE), vec![on(GUIDE)]),
                    In(down(QAM), vec![on(QAM)]),
                    In(up(QAM), vec![off(QAM)]),
                    In(up(GUIDE), vec![off(GUIDE)]),
                ],
            ),
            (
                "a local Quick Access stays with the shell",
                desktop(),
                vec![Set(local()), In(down(QAM), vec![]), In(up(QAM), vec![])],
            ),
            (
                "a Guide the shell keeps never reaches the shortcuts",
                vec![guide_a],
                vec![
                    Set(local()),
                    In(down(GUIDE), vec![Action::Flush]),
                    Release(vec![]),
                    In(down(A), vec![]),
                ],
            ),
            (
                "the ring without the mask leaves the host its input",
                desktop(),
                vec![
                    Set(Gates {
                        ring_nav: true,
                        ..host()
                    }),
                    In(down(A), vec![on(A)]),
                    In(PadInput::Analog, vec![Forward]),
                ],
            ),
            (
                "under the ring the host sees nothing",
                desktop(),
                vec![
                    Set(Gates {
                        ring_nav: true,
                        ..masked()
                    }),
                    In(down(A), vec![]),
                    In(PadInput::Analog, vec![]),
                ],
            ),
            (
                "8fe3637a3: a released capture masks the pads, then adopts what is held",
                desktop(),
                vec![
                    In(down(A), vec![on(A)]),
                    Set(masked()),
                    Release(vec![(A, false)]),
                    In(up(A), vec![]),
                    In(down(GUIDE), vec![]),
                    In(PadInput::Analog, vec![]),
                    In(click(true), vec![]),
                    Set(host()),
                    Adopt(vec![X], false, true),
                    In(up(X), vec![off(X)]),
                ],
            ),
            (
                "the escape chord pressed again under the mask toggles capture",
                desktop(),
                vec![
                    Set(masked()),
                    In(down(LB), vec![]),
                    In(down(RB), vec![]),
                    In(down(START), vec![]),
                    In(down(SELECT), vec![]),
                    In(up(SELECT), vec![fire(SlotId::ReleaseInput)]),
                    In(up(LB), vec![]),
                ],
            ),
            (
                "bb43ccda7: the escape hold outlives the mask it raises",
                deck(),
                vec![
                    In(down(LB), vec![on(LB)]),
                    In(down(RB), vec![on(RB)]),
                    In(down(START), vec![on(START)]),
                    In(down(SELECT), vec![]),
                    Set(masked()),
                    Release(vec![(LB, false), (RB, false), (START, false)]),
                    Wait(HOLD, vec![fire(SlotId::EndStream)]),
                    In(up(SELECT), vec![]),
                ],
            ),
            (
                "b42b33132: a local Guide holds its pad off the wire until it lifts",
                desktop(),
                vec![
                    Set(local()),
                    In(down(A), vec![on(A)]),
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
                desktop(),
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
                "51adfbb8e: Select then A opens the ring and lifts the Select",
                desktop(),
                vec![
                    In(down(SELECT), vec![on(SELECT)]),
                    In(
                        down(A),
                        vec![
                            off(SELECT),
                            Action::Fire(shortcuts::Action::Ring, Phase::Once),
                        ],
                    ),
                    In(up(A), vec![]),
                    In(up(SELECT), vec![]),
                ],
            ),
            (
                "51adfbb8e: under the ring the next A is the game's again",
                desktop(),
                vec![
                    In(down(SELECT), vec![on(SELECT)]),
                    In(
                        down(A),
                        vec![
                            off(SELECT),
                            Action::Fire(shortcuts::Action::Ring, Phase::Once),
                        ],
                    ),
                    Set(Gates {
                        masked: true,
                        ring_nav: true,
                        ..host()
                    }),
                    Release(vec![]),
                    In(up(A), vec![]),
                    In(up(SELECT), vec![]),
                    Set(host()),
                    Adopt(vec![], false, true),
                    In(down(A), vec![on(A)]),
                    In(up(A), vec![off(A)]),
                ],
            ),
            (
                "with the guide hold, a held-back Select goes with the ring chord",
                deck(),
                vec![
                    In(down(SELECT), vec![]),
                    In(
                        down(A),
                        vec![Action::Fire(shortcuts::Action::Ring, Phase::Once)],
                    ),
                    In(up(A), vec![]),
                    In(up(SELECT), vec![]),
                ],
            ),
            (
                "Select then X steps the stats; X stays off the wire",
                desktop(),
                vec![
                    In(down(SELECT), vec![on(SELECT)]),
                    In(down(X), vec![off(SELECT), fire(SlotId::Stats)]),
                    In(up(X), vec![]),
                ],
            ),
            (
                "a05b711ac: with no ring to open, Select then A is the game's",
                no_ring(),
                vec![In(down(SELECT), vec![on(SELECT)]), In(down(A), vec![on(A)])],
            ),
            (
                "a Select tap goes out on release, its lift later",
                deck(),
                vec![
                    In(down(SELECT), vec![]),
                    Wait(TAP_PRESS, vec![]),
                    In(up(SELECT), vec![on(SELECT)]),
                    Wait(TAP_PRESS, vec![off(SELECT)]),
                ],
            ),
            (
                "a long Select hold holds Guide down until Select lifts",
                deck(),
                vec![
                    In(down(SELECT), vec![]),
                    Wait(HOLD, vec![on(GUIDE)]),
                    In(up(SELECT), vec![off(GUIDE)]),
                ],
            ),
            (
                "a Select released past the key tap taps Guide",
                deck(),
                vec![
                    In(down(SELECT), vec![]),
                    Wait(KEY_TAP, vec![]),
                    In(up(SELECT), vec![on(GUIDE)]),
                    Wait(TAP_PRESS, vec![off(GUIDE)]),
                ],
            ),
            (
                "an exit-chord partner keeps the Select held back until it lifts",
                deck(),
                vec![
                    In(down(SELECT), vec![]),
                    Wait(KEY_TAP * 2, vec![]),
                    In(down(START), vec![on(START)]),
                    Wait(HOLD, vec![]),
                    In(up(SELECT), vec![on(SELECT)]),
                    Wait(TAP_PRESS, vec![off(SELECT)]),
                ],
            ),
            (
                "a press while Select is held back makes the Select real first",
                deck(),
                vec![
                    In(down(SELECT), vec![]),
                    In(down(Y), vec![on(SELECT), on(Y)]),
                    In(up(SELECT), vec![off(SELECT)]),
                ],
            ),
            (
                "the mask's flush lifts a shortcut's held Guide",
                deck(),
                vec![
                    In(down(SELECT), vec![]),
                    Wait(HOLD, vec![on(GUIDE)]),
                    Set(masked()),
                    Release(vec![(GUIDE, false)]),
                    In(up(SELECT), vec![off(GUIDE)]),
                ],
            ),
        ];
        for (name, bindings, steps) in rows {
            walk(name, bindings, steps);
        }
    }

    /// A Quick Access hold on a pad without the button holds Guide, then A, and lets go of
    /// both: A first, never before it went down.
    #[test]
    fn a_quick_access_hold_presses_its_chord_in_order() {
        let qam = Binding {
            trigger: Trigger::Pad {
                buttons: vec![START],
                hold: true,
            },
            action: shortcuts::Action::Slot(SlotId::Qam),
        };
        walk(
            "hold Start for Quick Access, released at once",
            vec![qam],
            vec![
                In(down(START), vec![]),
                Wait(HOLD, vec![on(GUIDE)]),
                In(up(START), vec![]),
                Wait(TAP_PRESS, vec![on(A), off(A), off(GUIDE)]),
            ],
        );
    }

    /// New bindings, or a slot closing, leave nothing down: the held Guide comes up, and the
    /// Select still held reaches the game as a press the new bindings never saw.
    #[test]
    fn a_rebind_lets_go_of_what_the_old_bindings_held() {
        let t = Instant::now();
        let mut pad = PadRoute::new(&deck(), wire::quick_access_presses(false));
        assert_eq!(route(down(SELECT), &host(), &mut pad, t), []);
        assert_eq!(pad.poll(&host(), t + HOLD), [on(GUIDE)]);
        assert_eq!(pad.rebind(&desktop(), &host(), t + HOLD), [off(GUIDE)]);
        assert_eq!(
            route(up(SELECT), &host(), &mut pad, t + HOLD),
            [off(SELECT)]
        );
        assert!(pad.release().is_empty());
    }

    /// #2129: the exit chord in every press order, at three paces, released either way round.
    /// Guide never reaches the host, Select at most as one plain hold, and End stream fires
    /// once. The worker polls every 10 ms.
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
        let bindings = deck();
        let gates = host();
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
                    let mut pad = PadRoute::new(&bindings, wire::quick_access_presses(false));
                    let mut got = Vec::new();
                    let mut now = t0;
                    let end = held + gap * 3 + Duration::from_millis(200);
                    let mut next = steps.iter().peekable();
                    while now <= end {
                        while let Some(&&(at, input)) = next.peek() {
                            if at > now {
                                break;
                            }
                            next.next();
                            got.extend(route(input, &gates, &mut pad, now));
                        }
                        got.extend(pad.poll(&gates, now));
                        now += tick;
                    }
                    let at = format!("{order:?} lifted {lift:?}, {gap:?} apart: {got:?}");
                    let wired = |bit| {
                        got.iter()
                            .any(|a| matches!(a, Action::Wire { bit: b, .. } if *b == bit))
                    };
                    assert!(!wired(GUIDE), "Guide: {at}");
                    let select: Vec<_> = got
                        .iter()
                        .filter(|a| matches!(a, Action::Wire { bit, .. } if *bit == SELECT))
                        .collect();
                    let plain = [&on(SELECT), &off(SELECT)];
                    assert!(select.is_empty() || select == plain, "Select: {at}");
                    let fired: Vec<_> = got
                        .iter()
                        .filter(|a| matches!(a, Action::Fire(..)))
                        .collect();
                    assert_eq!(fired, [&fire(SlotId::EndStream)], "{at}");
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
                let pad = &mut PadRoute::new(&desktop(), &[]);
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
        let mut pad = PadRoute::new(&desktop(), &[]);
        assert_eq!(route(down(B), &gates, &mut pad, ms(30)), []);
        assert!(pad.adopt(vec![B], false, true));
        assert_eq!(route(up(B), &host(), &mut pad, ms(120)), [off(B)]);
    }

    /// a05b711ac: a raw Steam Controller runs the mask, system buttons and ring chord itself.
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
        let ring = Gates {
            ring_chord: true,
            ..host()
        };
        assert_eq!(
            ring.sc2(),
            Sc2Gate {
                chords: true,
                ..none
            }
        );
        assert!(ring_on_select_a(&desktop()));
        assert!(!ring_on_select_a(&no_ring()));
    }
}
