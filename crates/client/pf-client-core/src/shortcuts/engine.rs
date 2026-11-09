//! The shortcut engine: one device's presses and releases in, what the host and the client
//! get out. Pure: the caller owns the clock and the wire.

use super::model::{Action, ActionKind, Binding, Device};
use punktfunk_core::input::key_vk;
use std::time::{Duration, Instant};

/// A held trigger fires after this long.
pub const HOLD: Duration = Duration::from_millis(1000);
/// A held-back tap's release follows its press by this, so the two don't fold into one
/// pad state on the wire.
pub const TAP_PRESS: Duration = Duration::from_millis(50);
/// A key-like hold released after this long is a tap of its key, not of the button.
pub const KEY_TAP: Duration = Duration::from_millis(350);

/// A pad's wire bit, or a key's VK with left and right modifiers folded onto
/// `key_vk("ctrl" | "alt" | "shift")`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Press(u32),
    Release(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Once,
    /// A key-like action held: down on the host until [`Phase::Up`].
    Down,
    Up,
    /// A key-like action pressed and released at once.
    Tap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    /// Send this transition on, as input to the host (it obeys the input mask).
    Forward(u32, bool),
    Fire(Action, Phase),
    /// Release these on the host, mask or not: the rest of a chord that fired.
    Lift(Vec<u32>),
}

struct Rule {
    ids: Vec<u32>,
    hold: bool,
    action: Action,
}

struct Armed {
    since: Instant,
    hold: usize,
    /// The press with the same buttons: it fires when the chord lifts before [`HOLD`].
    twin: Option<usize>,
}

/// One pad's or the keyboard's shortcuts, as the rules of `design/client-shortcuts.md` §4.
/// Feed every transition to [`Engine::step`], call [`Engine::tick`] on every poll (late only
/// delays), and [`Engine::flush`] when the device goes away. Pass only available actions.
pub struct Engine {
    device: Device,
    rules: Vec<Rule>,
    /// Physically down, in press order.
    down: Vec<u32>,
    /// Down on the host because of a [`Out::Forward`].
    forwarded: Vec<u32>,
    /// Pressed, not sent: a held chord's presses, only while its hold is armed.
    held_back: Vec<u32>,
    armed: Option<Armed>,
    /// Buttons of a chord that fired: their releases are dropped.
    claimed: Vec<u32>,
    /// A key-like hold that went down, and the buttons whose first lift ends it.
    key_down: Option<(Action, Vec<u32>)>,
    /// A tap's release, due at the instant.
    owed: Vec<(Instant, u32)>,
}

impl Engine {
    /// Keeps `device`'s bindings the engine can act on: no unknown actions, no keyboard
    /// holds, and no one-button pad press (that tap is the game's).
    pub fn new(bindings: &[Binding], device: Device) -> Engine {
        let rules = bindings
            .iter()
            .filter(|b| b.trigger.device() == device && !matches!(b.action, Action::Unknown(_)))
            .map(|b| Rule {
                ids: b.trigger.ids(),
                hold: b.trigger.hold(),
                action: b.action.clone(),
            })
            .filter(|r| match device {
                Device::Pad => r.hold || r.ids.len() >= 2,
                Device::Keyboard => !r.hold && !r.ids.is_empty(),
            })
            .collect();
        Engine {
            device,
            rules,
            down: Vec::new(),
            forwarded: Vec::new(),
            held_back: Vec::new(),
            armed: None,
            claimed: Vec::new(),
            key_down: None,
            owed: Vec::new(),
        }
    }

    /// One transition at `now`. A tick that was due runs first.
    pub fn step(&mut self, ev: Event, now: Instant) -> Vec<Out> {
        let mut out = self.tick(now);
        match ev {
            Event::Press(b) => self.press(b, now, &mut out),
            Event::Release(b) => self.release(b, now, &mut out),
        }
        self.settle(now, &mut out);
        out
    }

    /// Owed tap releases and an armed hold that came due.
    pub fn tick(&mut self, now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        self.owed.retain(|&(due, b)| {
            let keep = now < due;
            if !keep {
                out.push(Out::Forward(b, false));
            }
            keep
        });
        if let Some(armed) = &self.armed {
            if now.saturating_duration_since(armed.since) >= HOLD {
                let hold = armed.hold;
                let phase = match self.rules[hold].action.kind() {
                    ActionKind::Once => Phase::Once,
                    ActionKind::KeyLike => Phase::Down,
                };
                self.fire(hold, phase, &mut out);
            }
        }
        out
    }

    /// The device is gone: a key-like hold comes up, and the host lets go of everything
    /// this engine sent down or still owes. Forgets every press.
    pub fn flush(&mut self) -> Vec<Out> {
        let mut out = Vec::new();
        if let Some((action, _)) = self.key_down.take() {
            out.push(Out::Fire(action, Phase::Up));
        }
        let mut lift: Vec<u32> = self.owed.drain(..).map(|(_, b)| b).collect();
        lift.append(&mut self.forwarded);
        if !lift.is_empty() {
            out.push(Out::Lift(lift));
        }
        self.down.clear();
        self.held_back.clear();
        self.claimed.clear();
        self.armed = None;
        out
    }

    fn press(&mut self, b: u32, now: Instant, out: &mut Vec<Out>) {
        // Key repeat, or a press the engine already counts.
        if self.down.contains(&b) {
            return;
        }
        if let Some(i) = self.owed.iter().position(|&(_, o)| o == b) {
            self.owed.remove(i);
            out.push(Out::Forward(b, false));
        }
        self.down.push(b);
        self.armed = None;
        // While a key-like hold is down, a press is the host's: Guide+A is Steam's chord.
        if !self.is_modifier(b) && self.key_down.is_none() {
            let press = self.exact(false);
            let hold = self.exact(true);
            match (press, hold) {
                (Some(p), None) => {
                    let phase = match self.rules[p].action.kind() {
                        ActionKind::Once => Phase::Once,
                        ActionKind::KeyLike => Phase::Tap,
                    };
                    return self.fire(p, phase, out);
                }
                (twin, Some(hold)) => {
                    self.held_back.push(b);
                    self.armed = Some(Armed {
                        since: now,
                        hold,
                        twin,
                    });
                    return;
                }
                (None, None) => {}
            }
        }
        // A partner that completes nothing makes a held-back press real, sent first.
        self.send_held_back(out);
        self.forwarded.push(b);
        out.push(Out::Forward(b, true));
    }

    fn release(&mut self, b: u32, now: Instant, out: &mut Vec<Out>) {
        let Some(i) = self.down.iter().position(|&d| d == b) else {
            // Pressed before this engine: the host may hold it.
            out.push(Out::Forward(b, false));
            return;
        };
        self.down.remove(i);
        if self
            .key_down
            .as_ref()
            .is_some_and(|(_, ids)| ids.contains(&b))
        {
            let (action, _) = self.key_down.take().expect("checked above");
            out.push(Out::Fire(action, Phase::Up));
        }
        if let Some(c) = self.claimed.iter().position(|&x| x == b) {
            self.claimed.remove(c);
            return;
        }
        // Armed means down was the hold's chord, so `b` is one of its buttons.
        if let Some(armed) = self.armed.take() {
            if let Some(twin) = armed.twin {
                let phase = match self.rules[twin].action.kind() {
                    ActionKind::Once => Phase::Once,
                    ActionKind::KeyLike => Phase::Tap,
                };
                return self.fire(twin, phase, out);
            }
            let key_like = self.rules[armed.hold].action.kind() == ActionKind::KeyLike;
            if key_like && now.saturating_duration_since(armed.since) >= KEY_TAP {
                return self.fire(armed.hold, Phase::Tap, out);
            }
        }
        if let Some(h) = self.held_back.iter().position(|&x| x == b) {
            self.held_back.remove(h);
            out.push(Out::Forward(b, true));
            self.owed.push((now + TAP_PRESS, b));
            return;
        }
        self.forwarded.retain(|&x| x != b);
        out.push(Out::Forward(b, false));
    }

    /// The rule fires: the host lets go of the chord, the held-back press is never sent,
    /// and every button still down is the chord's until it lifts.
    fn fire(&mut self, rule: usize, phase: Phase, out: &mut Vec<Out>) {
        let lift = std::mem::take(&mut self.forwarded);
        if !lift.is_empty() {
            out.push(Out::Lift(lift));
        }
        let action = self.rules[rule].action.clone();
        out.push(Out::Fire(action.clone(), phase));
        if phase == Phase::Down {
            self.key_down = Some((action, self.rules[rule].ids.clone()));
        }
        self.held_back.clear();
        self.armed = None;
        for &b in &self.down {
            if !self.claimed.contains(&b) {
                self.claimed.push(b);
            }
        }
    }

    /// A press stays held back only while its hold is armed. When a release disarms it, a
    /// hold still exactly down (a nested one) arms from `now`; otherwise the press goes out.
    fn settle(&mut self, now: Instant, out: &mut Vec<Out>) {
        if self.armed.is_some() || self.held_back.is_empty() {
            return;
        }
        match self.exact(true) {
            Some(hold) => {
                self.armed = Some(Armed {
                    since: now,
                    hold,
                    twin: self.exact(false),
                })
            }
            None => self.send_held_back(out),
        }
    }

    fn send_held_back(&mut self, out: &mut Vec<Out>) {
        for h in std::mem::take(&mut self.held_back) {
            self.forwarded.push(h);
            out.push(Out::Forward(h, true));
        }
    }

    /// The rule whose buttons are exactly what is down.
    fn exact(&self, hold: bool) -> Option<usize> {
        self.rules.iter().position(|r| {
            r.hold == hold && r.ids.len() == self.down.len() && subset(&self.down, &r.ids)
        })
    }

    /// A modifier never completes a keyboard shortcut and is never held back.
    fn is_modifier(&self, b: u32) -> bool {
        self.device == Device::Keyboard
            && ["ctrl", "alt", "shift"]
                .iter()
                .any(|m| key_vk(m).map(u32::from) == Some(b))
    }
}

fn subset(small: &[u32], big: &[u32]) -> bool {
    small.iter().all(|b| big.contains(b))
}
