//! Keys and pointer ([`InputKind`] other than scroll and pads) → what a Linux back end emits.
//! Scroll has its own plan ([`crate::scroll`]).
//!
//! Pure and ungated like [`crate::scroll`]: tests on any platform assert what the injectors
//! execute. Each device owns one [`Plan`], born with it. The plan holds what the device has
//! pressed, so it passes key transitions only and lists the releases the device owes before it
//! closes ([`Plan::release`]). A client forwards its OS auto-repeat as more downs; a Wayland app
//! repeats a held key itself, and Hyprland re-runs a bind on every press and can swallow the
//! release. The absolute extent packed in `flags` is decoded here, once ([`Abs::of`]).

use crate::held::HeldInput;
use crate::keymap::{gs_button_to_evdev, vk_to_evdev};
use punktfunk_core::input::{InputEvent, InputKind};

/// Largest normalized absolute position. A client may send `x == w` (a touch on the far edge);
/// mapped as 1.0 that lands one pixel past the head, on the neighbouring monitor.
pub(crate) const ABS_EDGE: f32 = 1.0 - 1.0 / 65536.0;

/// An absolute sample (pointer or touch): client pixels on a `w×h` surface, both nonzero, on
/// screen `screen` of the session ([`InputEvent::screen`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Abs {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub screen: u8,
}

impl Abs {
    /// `flags` packs the client surface as `(w << 16) | h`. `None` for a zero-sized surface,
    /// which maps nowhere.
    pub fn of(ev: &InputEvent) -> Option<Abs> {
        let (w, h) = (ev.flags >> 16, ev.flags & 0xffff);
        (w > 0 && h > 0).then_some(Abs {
            x: ev.x,
            y: ev.y,
            w,
            h,
            screen: ev.screen(),
        })
    }

    /// The sample in the surface's integer units, for a protocol that takes a position with its
    /// extent (wlr). The far edge belongs to the neighbouring head, so the last unit is `w - 1`.
    pub fn in_extent(&self) -> (u32, u32) {
        let clamp = |v: i32, n: u32| v.clamp(0, n as i32 - 1) as u32;
        (clamp(self.x, self.w), clamp(self.y, self.h))
    }

    /// The sample as a fraction of the surface, for a protocol in compositor coordinates (KWin,
    /// libei). The same far-edge rule as [`Self::in_extent`], at [`ABS_EDGE`]'s finer step.
    pub fn normalized(&self) -> (f64, f64) {
        let edge = f64::from(ABS_EDGE);
        let norm = |v: i32, n: u32| (f64::from(v) / f64::from(n)).clamp(0.0, edge);
        (norm(self.x, self.w), norm(self.y, self.h))
    }
}

/// What a device emits for one wire event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    /// Relative pointer motion, client pixels.
    Motion {
        dx: i32,
        dy: i32,
    },
    Absolute(Abs),
    /// A `BTN_*` code.
    Button {
        evdev: u32,
        down: bool,
    },
    /// An evdev key, once per change of its state: it goes down with the first wire key that
    /// holds it and up with the last. xkb counts presses per key, so it may feed on every one.
    Key {
        evdev: u16,
        down: bool,
    },
    /// `id` is the client's touch id.
    TouchDown {
        id: u32,
        at: Abs,
    },
    TouchMove {
        id: u32,
        at: Abs,
    },
    TouchUp {
        id: u32,
    },
    /// One committed Unicode scalar.
    Text(u32),
}

/// One device's keys and pointer: what it holds and what it must emit next.
#[derive(Default)]
pub(crate) struct Plan {
    /// Presses the device holds. Keys by the low byte, the VK every injector takes.
    held: HeldInput,
}

impl Plan {
    /// What the device emits for `ev`, noting what it now holds. `None` for a repeat, a stale
    /// key up, a key another held VK keeps down (VK_SHIFT beside VK_LSHIFT), an unmapped
    /// code, a zero-sized surface, scroll and pads. A touch id is down once: a lost up makes
    /// the next down a move, and a move or up for an id not down is `None`.
    pub fn step(&mut self, ev: &InputEvent) -> Option<Op> {
        let down = matches!(ev.kind, InputKind::KeyDown | InputKind::MouseButtonDown);
        Some(match ev.kind {
            InputKind::MouseMove => Op::Motion { dx: ev.x, dy: ev.y },
            InputKind::MouseMoveAbs => Op::Absolute(Abs::of(ev)?),
            InputKind::MouseButtonDown | InputKind::MouseButtonUp => {
                let evdev = gs_button_to_evdev(ev.code)?;
                self.held.note(ev);
                Op::Button { evdev, down }
            }
            InputKind::KeyDown | InputKind::KeyUp => {
                let Some(evdev) = vk_to_evdev(ev.code as u8) else {
                    tracing::debug!(vk = ev.code, "unmapped VK keycode — dropped");
                    return None;
                };
                let vk = ev.code & 0xff;
                if !self.held.note(&InputEvent { code: vk, ..*ev }) {
                    return None;
                }
                let aliased = self
                    .held
                    .keys()
                    .any(|k| k != vk && vk_to_evdev(k as u8) == Some(evdev));
                if aliased {
                    return None;
                }
                Op::Key { evdev, down }
            }
            InputKind::TouchDown => {
                let at = Abs::of(ev)?;
                if self.held.touching(ev.code) {
                    Op::TouchMove { id: ev.code, at }
                } else if self.held.note(ev) {
                    Op::TouchDown { id: ev.code, at }
                } else {
                    return None;
                }
            }
            InputKind::TouchMove => {
                let at = Abs::of(ev)?;
                if !self.held.touching(ev.code) {
                    return None;
                }
                Op::TouchMove { id: ev.code, at }
            }
            InputKind::TouchUp => {
                if !self.held.note(ev) {
                    return None;
                }
                Op::TouchUp { id: ev.code }
            }
            InputKind::TextInput => Op::Text(ev.code),
            InputKind::MouseScroll
            | InputKind::Scroll
            | InputKind::GamepadState
            | InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::GamepadRemove
            | InputKind::GamepadArrival => return None,
        })
    }

    /// The ups for everything still held, planned like the client's own. Leaves nothing held.
    pub fn release(&mut self) -> Vec<Op> {
        self.lift(true, true)
    }

    /// [`Self::release`] for keys, or for the pointer (buttons and touches), only.
    pub fn lift(&mut self, keys: bool, pointer: bool) -> Vec<Op> {
        let ups = self.held.clone().release_classes(keys, pointer);
        ups.iter().filter_map(|up| self.step(up)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: u16 = 30;
    const KEY_TAB: u16 = 15;
    const KEY_LEFTCTRL: u16 = 29;
    const KEY_LEFTSHIFT: u16 = 42;
    const VK_A: u32 = 0x41;
    const VK_TAB: u32 = 0x09;
    const VK_SHIFT: u32 = 0x10;
    const VK_LSHIFT: u32 = 0xA0;
    const VK_LCONTROL: u32 = 0xA2;
    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;
    const BTN_MIDDLE: u32 = 0x112;

    fn ev(kind: InputKind, code: u32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x: 0,
            y: 0,
            flags: 0,
        }
    }

    fn down(vk: u32) -> InputEvent {
        ev(InputKind::KeyDown, vk)
    }

    fn up(vk: u32) -> InputEvent {
        ev(InputKind::KeyUp, vk)
    }

    fn at(kind: InputKind, code: u32, (x, y): (i32, i32), (w, h): (u32, u32)) -> InputEvent {
        InputEvent {
            x,
            y,
            flags: (w << 16) | h,
            ..ev(kind, code)
        }
    }

    fn key(evdev: u16, down: bool) -> Op {
        Op::Key { evdev, down }
    }

    fn button(evdev: u32, down: bool) -> Op {
        Op::Button { evdev, down }
    }

    fn run(plan: &mut Plan, events: &[InputEvent]) -> Vec<Op> {
        events.iter().filter_map(|e| plan.step(e)).collect()
    }

    fn sorted(mut ops: Vec<Op>) -> Vec<String> {
        let mut out: Vec<String> = ops.drain(..).map(|o| format!("{o:?}")).collect();
        out.sort();
        out
    }

    /// Each wire stream, on a fresh device, and the ops it plans.
    #[test]
    fn wire_streams_plan_to_these_ops() {
        let sample = Abs {
            x: 10,
            y: 20,
            w: 100,
            h: 50,
            screen: 0,
        };
        let rows: Vec<(&str, Vec<InputEvent>, Vec<Op>)> = vec![
            (
                "an auto-repeat presses once",
                vec![down(VK_A), down(VK_A), down(VK_A), up(VK_A)],
                vec![key(KEY_A, true), key(KEY_A, false)],
            ),
            ("a stale up stops here", vec![up(VK_A)], vec![]),
            (
                "a code past the low byte is the same key",
                vec![down(VK_A), down(0x100 | VK_A), up(0x100 | VK_A), up(VK_A)],
                vec![key(KEY_A, true), key(KEY_A, false)],
            ),
            (
                "a ring's Shift under a held Left Shift neither presses nor lifts it",
                vec![
                    down(VK_LSHIFT),
                    down(VK_SHIFT),
                    down(VK_A),
                    up(VK_A),
                    up(VK_SHIFT),
                    down(VK_A),
                    up(VK_LSHIFT),
                ],
                vec![
                    key(KEY_LEFTSHIFT, true),
                    key(KEY_A, true),
                    key(KEY_A, false),
                    key(KEY_A, true),
                    key(KEY_LEFTSHIFT, false),
                ],
            ),
            (
                "the last of two VKs on one key lifts it, whichever it is",
                vec![down(VK_SHIFT), down(VK_LSHIFT), up(VK_SHIFT), up(VK_LSHIFT)],
                vec![key(KEY_LEFTSHIFT, true), key(KEY_LEFTSHIFT, false)],
            ),
            (
                "an unmapped key goes nowhere",
                vec![down(0xFF), up(0xFF)],
                vec![],
            ),
            (
                "GameStream numbers the middle button 2 and the right 3",
                (1..=3)
                    .map(|b| ev(InputKind::MouseButtonDown, b))
                    .chain([ev(InputKind::MouseButtonUp, 3)])
                    .collect(),
                vec![
                    button(BTN_LEFT, true),
                    button(BTN_MIDDLE, true),
                    button(BTN_RIGHT, true),
                    button(BTN_RIGHT, false),
                ],
            ),
            (
                "a repeated button press passes",
                vec![ev(InputKind::MouseButtonDown, 1); 2],
                vec![button(BTN_LEFT, true); 2],
            ),
            (
                "relative motion passes as sent",
                vec![InputEvent {
                    x: 5,
                    y: -3,
                    ..ev(InputKind::MouseMove, 0)
                }],
                vec![Op::Motion { dx: 5, dy: -3 }],
            ),
            (
                "an absolute sample keeps the client's extent for the head pick",
                vec![at(InputKind::MouseMoveAbs, 0, (10, 20), (100, 50))],
                vec![Op::Absolute(sample)],
            ),
            (
                "a zero-sized surface maps nowhere",
                vec![
                    at(InputKind::MouseMoveAbs, 0, (10, 20), (0, 50)),
                    at(InputKind::TouchDown, 7, (10, 20), (100, 0)),
                ],
                vec![],
            ),
            (
                "a touch carries its id",
                vec![
                    at(InputKind::TouchDown, 7, (10, 20), (100, 50)),
                    at(InputKind::TouchMove, 7, (10, 20), (100, 50)),
                    ev(InputKind::TouchUp, 7),
                ],
                vec![
                    Op::TouchDown { id: 7, at: sample },
                    Op::TouchMove { id: 7, at: sample },
                    Op::TouchUp { id: 7 },
                ],
            ),
            (
                "a down for a live id moves it, so a lost up never doubles a down",
                vec![
                    at(InputKind::TouchDown, 7, (10, 20), (100, 50)),
                    at(InputKind::TouchDown, 7, (10, 20), (100, 50)),
                    ev(InputKind::TouchUp, 7),
                ],
                vec![
                    Op::TouchDown { id: 7, at: sample },
                    Op::TouchMove { id: 7, at: sample },
                    Op::TouchUp { id: 7 },
                ],
            ),
            (
                "a move or up for an id that is not down stops here",
                vec![
                    at(InputKind::TouchMove, 7, (10, 20), (100, 50)),
                    ev(InputKind::TouchUp, 7),
                    at(InputKind::TouchDown, 8, (10, 20), (100, 50)),
                    ev(InputKind::TouchUp, 8),
                    ev(InputKind::TouchUp, 8),
                    at(InputKind::TouchMove, 8, (10, 20), (100, 50)),
                ],
                vec![Op::TouchDown { id: 8, at: sample }, Op::TouchUp { id: 8 }],
            ),
            (
                "committed text passes",
                vec![ev(InputKind::TextInput, 'é' as u32)],
                vec![Op::Text('é' as u32)],
            ),
            (
                "scroll is the scroll plan's, pads are the pads'",
                vec![
                    ev(InputKind::Scroll, 0),
                    ev(InputKind::MouseScroll, 0),
                    ev(InputKind::GamepadButton, 0),
                ],
                vec![],
            ),
        ];
        for (name, events, want) in rows {
            let mut plan = Plan::default();
            assert_eq!(run(&mut plan, &events), want, "{name}");
        }
    }

    /// `x == w` is a touch on the far edge. Both protocol shapes keep it on the streamed head,
    /// and agree with each other on every pixel inside it.
    #[test]
    fn the_far_edge_stays_on_the_streamed_head() {
        let edge = Abs {
            x: 1920,
            y: 1080,
            w: 1920,
            h: 1080,
            screen: 0,
        };
        assert_eq!(edge.in_extent(), (1919, 1079));
        let (nx, ny) = edge.normalized();
        assert!(nx < 1.0 && ny < 1.0);
        assert_eq!((nx, ny), (f64::from(ABS_EDGE), f64::from(ABS_EDGE)));
        let before = Abs {
            x: -5,
            y: -1,
            ..edge
        };
        assert_eq!(before.in_extent(), (0, 0));
        assert_eq!(before.normalized(), (0.0, 0.0));
        for x in 0..1920 {
            let a = Abs { x, ..edge };
            assert_eq!(a.in_extent().0, x as u32);
            assert_eq!(a.normalized().0, f64::from(x) / 1920.0);
        }
    }

    /// A lost up and a session's open press: the device lifts each once before it closes, a
    /// key two VKs hold included, and a later down of the same key presses again.
    #[test]
    fn a_closing_device_releases_what_it_holds_once() {
        let mut plan = Plan::default();
        run(
            &mut plan,
            &[
                down(VK_A),
                down(VK_A),
                down(VK_LSHIFT),
                down(VK_SHIFT),
                ev(InputKind::MouseButtonDown, 3),
                at(InputKind::TouchDown, 7, (1, 1), (2, 2)),
                down(VK_TAB),
                up(VK_TAB),
            ],
        );
        assert_eq!(
            sorted(plan.release()),
            sorted(vec![
                key(KEY_A, false),
                key(KEY_LEFTSHIFT, false),
                button(BTN_RIGHT, false),
                Op::TouchUp { id: 7 },
            ])
        );
        assert!(plan.release().is_empty());
        assert_eq!(run(&mut plan, &[down(VK_A)]), [key(KEY_A, true)]);
    }

    /// The pointer lets go and the keys stay: a re-aimed wlr pointer drops its buttons only.
    #[test]
    fn lifting_the_pointer_keeps_the_keys() {
        let mut plan = Plan::default();
        run(
            &mut plan,
            &[down(VK_LCONTROL), ev(InputKind::MouseButtonDown, 1)],
        );
        assert_eq!(plan.lift(false, true), [button(BTN_LEFT, false)]);
        assert_eq!(plan.release(), [key(KEY_LEFTCTRL, false)]);
    }

    /// Ctrl held through an Alt+Tab-style focus change: the compositor moves focus, the
    /// client keeps Ctrl down. Ctrl presses once and its one up passes. A device that opens
    /// mid-hold (a Desktop↔Game switch) holds nothing: the old device lifts Ctrl, and the
    /// client's next Ctrl repeat presses it on the new one.
    #[test]
    fn a_modifier_held_across_a_focus_change_presses_once() {
        let mut plan = Plan::default();
        let ops = run(
            &mut plan,
            &[
                down(VK_LCONTROL),
                down(VK_TAB),
                up(VK_TAB),
                down(VK_LCONTROL),
                down(VK_TAB),
                up(VK_TAB),
            ],
        );
        assert_eq!(
            ops,
            [
                key(KEY_LEFTCTRL, true),
                key(KEY_TAB, true),
                key(KEY_TAB, false),
                key(KEY_TAB, true),
                key(KEY_TAB, false),
            ]
        );
        assert_eq!(plan.release(), [key(KEY_LEFTCTRL, false)]);
        let mut next = Plan::default();
        assert_eq!(
            run(&mut next, &[down(VK_LCONTROL), up(VK_LCONTROL)]),
            [key(KEY_LEFTCTRL, true), key(KEY_LEFTCTRL, false)]
        );
    }

    /// Stuck keys on Hyprland: a key held first, a modifier pressed during the hold, and the
    /// key's repeat matching a bind made Hyprland swallow the key's release. The repeat never
    /// reaches the compositor, so each key presses once and its release passes.
    #[test]
    fn a_chord_repeat_never_presses_twice() {
        let mut plan = Plan::default();
        let ops = run(
            &mut plan,
            &[
                down(VK_A),
                down(VK_LCONTROL),
                down(VK_A),
                down(VK_A),
                up(VK_A),
                up(VK_LCONTROL),
            ],
        );
        assert_eq!(
            ops,
            [
                key(KEY_A, true),
                key(KEY_LEFTCTRL, true),
                key(KEY_A, false),
                key(KEY_LEFTCTRL, false),
            ]
        );
        assert!(plan.release().is_empty());
    }

    /// GameStream control and native input each keep a `HeldInput` of their own and send its
    /// ups through the injector when a session ends. Those ups are ordinary events here: fed the
    /// same stream, they leave the plan holding nothing, whatever order they arrive in.
    #[test]
    fn the_ups_a_session_owes_drain_the_plan() {
        let stream = [
            down(VK_LSHIFT),
            down(VK_SHIFT),
            down(VK_A),
            down(0x100 | VK_A),
            ev(InputKind::MouseButtonDown, 1),
            at(InputKind::TouchDown, 7, (0, 0), (100, 50)),
        ];
        let mut session = HeldInput::default();
        for e in &stream {
            session.note(e);
        }
        let owed = session.release();
        assert!(session.is_empty());
        for order in [owed.clone(), owed.into_iter().rev().collect()] {
            let mut plan = Plan::default();
            run(&mut plan, &stream);
            let lifted = sorted(run(&mut plan, &order));
            assert_eq!(
                lifted,
                sorted(vec![
                    key(KEY_LEFTSHIFT, false),
                    key(KEY_A, false),
                    button(BTN_LEFT, false),
                    Op::TouchUp { id: 7 },
                ])
            );
            assert!(plan.release().is_empty(), "the plan holds nothing after");
        }
    }
}
