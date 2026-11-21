//! Keys, pointer, text and scroll → the CoreGraphics events the macOS injector posts.
//!
//! Pure: [`Planner::plan`] decides every field, so tests on any OS assert what `cgevent.rs`
//! posts; that file only builds and posts. The numbers are the CoreGraphics and IOKit values;
//! a macOS test pins them to the SDK's.
//!
//! - Keys: the wire's US-positional VK → the ANSI-positional macOS key code ([`mac_key`]).
//!   Every event carries the flags of the modifiers held, with the side bits a keyboard sets.
//!   A modifier is a flags-changed event; the client's auto-repeat is posted as auto-repeat.
//! - Pointer: every location lands on [`Desk::bounds`], relative motion from where the planner
//!   last put the pointer while the cursor read back still lags it. A press close to the last
//!   one in time and place counts up the click state; a move with a button held is a drag.
//! - Scroll: the shared mapper's libei plan (point distance, v120 clicks, stops) as pixel
//!   and line events; a phased wire gesture also carries the scroll phase.

use crate::held::HeldInput;
use crate::plan::Abs;
use crate::scroll::{ScrollBackend, ScrollMapper, ScrollOp, WheelAxis};
use punktfunk_core::input::scroll::{ScrollEvent, ScrollPhase};
use punktfunk_core::input::{InputEvent, InputKind};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// `CGEventType` values.
pub(crate) mod ty {
    pub const LEFT_DOWN: u32 = 1;
    pub const LEFT_UP: u32 = 2;
    pub const RIGHT_DOWN: u32 = 3;
    pub const RIGHT_UP: u32 = 4;
    pub const MOVED: u32 = 5;
    pub const LEFT_DRAGGED: u32 = 6;
    pub const RIGHT_DRAGGED: u32 = 7;
    pub const KEY_DOWN: u32 = 10;
    pub const KEY_UP: u32 = 11;
    pub const FLAGS_CHANGED: u32 = 12;
    pub const OTHER_DOWN: u32 = 25;
    pub const OTHER_UP: u32 = 26;
    pub const OTHER_DRAGGED: u32 = 27;
}

/// `CGEventFlags` masks, then `IOLLEvent.h`'s device-dependent bits (`NX_DEVICE*KEYMASK`),
/// which tell a left modifier from a right one.
pub(crate) mod flag {
    pub const ALPHA_SHIFT: u64 = 1 << 16;
    pub const SHIFT: u64 = 1 << 17;
    pub const CONTROL: u64 = 1 << 18;
    pub const ALTERNATE: u64 = 1 << 19;
    pub const COMMAND: u64 = 1 << 20;
    pub const L_CTRL: u64 = 0x0001;
    pub const L_SHIFT: u64 = 0x0002;
    pub const R_SHIFT: u64 = 0x0004;
    pub const L_CMD: u64 = 0x0008;
    pub const R_CMD: u64 = 0x0010;
    pub const L_ALT: u64 = 0x0020;
    pub const R_ALT: u64 = 0x0040;
    pub const R_CTRL: u64 = 0x2000;
}

/// `CGScrollPhase` values.
pub(crate) mod phase {
    pub const BEGAN: u32 = 1;
    pub const CHANGED: u32 = 2;
    pub const ENDED: u32 = 4;
    pub const CANCELLED: u32 = 8;
}

/// `kVK_Space`: the key a text event rides on.
pub(crate) const KVK_SPACE: u16 = 0x31;
const KVK_CAPS_LOCK: u16 = 0x39;

/// How far apart two presses land and still count up, in points: Windows' default
/// double-click rectangle.
const CLICK_SLOP: f64 = 4.0;

/// Known pointer locations kept: more than a back-to-back burst of moves leaves unapplied when
/// the cursor is read back.
const RECENT: usize = 64;

/// Wire buttons 1..=5 (left, middle, right, X1, X2) → `CGMouseButton`.
const BUTTONS: [u32; 5] = [0, 2, 1, 3, 4];

/// `(x, y, w, h)` in global display points, y down from the main display's top.
pub(crate) type Rect = (f64, f64, f64, f64);

/// What a pointer event reads off the desktop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Desk {
    /// Where the window server has the cursor now. It lags the events it has not applied.
    pub cursor: (f64, f64),
    /// The display absolute input maps into, and every posted location stays on.
    pub bounds: Rect,
}

/// One event to post, every field decided.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Post {
    /// `ty` is [`ty::KEY_DOWN`], [`ty::KEY_UP`] or [`ty::FLAGS_CHANGED`]; `code` is a `kVK_*`.
    Key {
        ty: u32,
        code: u16,
        flags: u64,
        repeat: bool,
    },
    /// One scalar as the Unicode string of a [`KVK_SPACE`] edge with no modifiers: an app that
    /// reads only the key code sees a space, and a held Command cannot turn it into a shortcut.
    Text { ch: char, down: bool },
    /// `button` is a `CGMouseButton`; `delta` is the motion an app reads, `clicks` the click
    /// state.
    Mouse {
        ty: u32,
        button: u32,
        at: (f64, f64),
        delta: (i32, i32),
        clicks: i64,
        flags: u64,
    },
    /// Wheel 1 is vertical, positive up; wheel 2 horizontal, positive left. In lines or points.
    Scroll {
        lines: bool,
        wheel1: i32,
        wheel2: i32,
        phase: u32,
        flags: u64,
    },
}

/// The latest press, which the next press of its button may count up from.
#[derive(Clone, Copy)]
struct Press {
    button: u32,
    at: (f64, f64),
    when: Instant,
    count: i64,
}

/// One injector's keys, buttons and scroll: what it holds and what it posts next.
pub(crate) struct Planner {
    /// Keys (low-byte VKs) and wire buttons pressed and not released.
    held: HeldInput,
    /// Caps Lock's lock, flipped by each press.
    caps: bool,
    /// `CGMouseButton`s down, one bit each.
    buttons: u32,
    press: Option<Press>,
    /// The host's double-click interval.
    double_click: Duration,
    /// The latest pointer locations known: posted, or read back and followed. Newest last.
    recent: VecDeque<(f64, f64)>,
    /// Libei's plan is the one in CGEvent's terms: point distance, v120 clicks, stops.
    scroll: ScrollMapper,
    /// The fraction of a point each precise axis owes.
    px: [f64; 2],
    /// 120ths of a line each wheel axis owes.
    wheel: [WheelAxis; 2],
    /// Axes inside a phased gesture. macOS phases one gesture over both axes.
    open: [bool; 2],
}

impl Planner {
    pub fn new(double_click: Duration) -> Self {
        Planner {
            held: HeldInput::default(),
            caps: false,
            buttons: 0,
            press: None,
            double_click,
            recent: VecDeque::with_capacity(RECENT),
            scroll: ScrollMapper::new(ScrollBackend::Libei),
            px: [0.0; 2],
            wheel: Default::default(),
            open: [false; 2],
        }
    }

    /// The events `ev` posts, noting what is now held. `desk` is read only by pointer events.
    pub fn plan(&mut self, ev: &InputEvent, now: Instant, desk: &dyn Fn() -> Desk) -> Vec<Post> {
        match ev.kind {
            InputKind::KeyDown | InputKind::KeyUp => self.key(ev).into_iter().collect(),
            InputKind::TextInput => text(ev.code),
            InputKind::MouseMove => {
                let d = desk();
                let (x, y) = self.pointer(d.cursor);
                let at = clamp_to(d.bounds, (x + f64::from(ev.x), y + f64::from(ev.y)));
                vec![self.motion(at, (ev.x, ev.y))]
            }
            InputKind::MouseMoveAbs => {
                let Some(abs) = Abs::of(ev) else {
                    return Vec::new();
                };
                let d = desk();
                let ((fx, fy), b) = (abs.normalized(), d.bounds);
                let at = clamp_to(b, (b.0 + fx * b.2, b.1 + fy * b.3));
                let (x, y) = self.pointer(d.cursor);
                let delta = ((at.0 - x).round() as i32, (at.1 - y).round() as i32);
                vec![self.motion(at, delta)]
            }
            InputKind::MouseButtonDown | InputKind::MouseButtonUp => {
                self.button(ev, now, desk).into_iter().collect()
            }
            InputKind::MouseScroll | InputKind::Scroll => {
                let wire = crate::scroll::from_legacy(ev).unwrap_or(*ev);
                let phased =
                    ScrollEvent::from_event(&wire).is_some_and(|se| se.phase != ScrollPhase::None);
                let ops = self.scroll.plan(ev);
                self.lower(ops, phased)
            }
            // Touch and pen stay off on macOS; pads are another plane.
            InputKind::TouchDown
            | InputKind::TouchMove
            | InputKind::TouchUp
            | InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::GamepadState
            | InputKind::GamepadRemove
            | InputKind::GamepadArrival => Vec::new(),
        }
    }

    /// When a held scroll stop falls due ([`Self::flush`]).
    pub fn due(&self) -> Option<Instant> {
        self.scroll.stop_due()
    }

    /// The scroll stops due by `now`.
    pub fn flush(&mut self, now: Instant) -> Vec<Post> {
        let ops = self.scroll.flush_due(now);
        self.lower(ops, false)
    }

    /// The ups for every key and button still held, then the end of an open scroll gesture.
    pub fn release(&mut self, now: Instant, desk: &dyn Fn() -> Desk) -> Vec<Post> {
        let ups = self.held.clone().release();
        let mut out: Vec<Post> = ups.iter().flat_map(|up| self.plan(up, now, desk)).collect();
        let ops = self.scroll.cancel_all();
        out.extend(self.lower(ops, false));
        out
    }

    /// `None` for an unmapped key, a stale release, a modifier's repeat, and a key another held
    /// VK keeps down (VK_SHIFT beside VK_LSHIFT).
    fn key(&mut self, ev: &InputEvent) -> Option<Post> {
        let vk = ev.code & 0xff;
        let code = mac_key(vk as u8)?;
        let down = ev.kind == InputKind::KeyDown;
        let changed = self.held.note(&InputEvent { code: vk, ..*ev });
        if !changed && !down {
            return None;
        }
        if (self.held.keys()).any(|k| k != vk && mac_key(k as u8) == Some(code)) {
            return None;
        }
        let flags_changed = |p: &Self| Post::Key {
            ty: ty::FLAGS_CHANGED,
            code,
            flags: p.flags(),
            repeat: false,
        };
        if code == KVK_CAPS_LOCK {
            // A lock flips on each press, one flags-changed event per flip.
            if !(down && changed) {
                return None;
            }
            self.caps = !self.caps;
            return Some(flags_changed(self));
        }
        if modifier(code).is_some() {
            return changed.then(|| flags_changed(self));
        }
        Some(Post::Key {
            ty: if down { ty::KEY_DOWN } else { ty::KEY_UP },
            code,
            flags: self.flags(),
            repeat: !changed,
        })
    }

    /// The modifier flags of every held key and the Caps Lock lock.
    fn flags(&self) -> u64 {
        let caps = if self.caps { flag::ALPHA_SHIFT } else { 0 };
        (self.held.keys())
            .filter_map(|vk| modifier(mac_key(vk as u8)?))
            .fold(caps, |f, m| f | m)
    }

    /// `None` for a button the wire does not define, a repeated press and a stale release.
    fn button(&mut self, ev: &InputEvent, now: Instant, desk: &dyn Fn() -> Desk) -> Option<Post> {
        let &button = BUTTONS.get(ev.code.checked_sub(1)? as usize)?;
        if !self.held.note(ev) {
            return None;
        }
        let down = ev.kind == InputKind::MouseButtonDown;
        let d = desk();
        let at = clamp_to(d.bounds, self.pointer(d.cursor));
        self.moved_to(at);
        let (press, release, _) = button_types(button);
        let clicks = if down {
            let count = match self.press {
                Some(p)
                    if p.button == button
                        && now.saturating_duration_since(p.when) <= self.double_click
                        && (p.at.0 - at.0).abs() <= CLICK_SLOP
                        && (p.at.1 - at.1).abs() <= CLICK_SLOP =>
                {
                    p.count + 1
                }
                _ => 1,
            };
            self.press = Some(Press {
                button,
                at,
                when: now,
                count,
            });
            self.buttons |= 1 << button;
            count
        } else {
            self.buttons &= !(1 << button);
            self.clicks_of(button)
        };
        Some(Post::Mouse {
            ty: if down { press } else { release },
            button,
            at,
            delta: (0, 0),
            clicks,
            flags: self.flags(),
        })
    }

    /// Where the pointer is: the newest known location when `cursor` is an older one the window
    /// server has not caught up from, else `cursor`, someone else's move, which becomes known.
    fn pointer(&mut self, cursor: (f64, f64)) -> (f64, f64) {
        let near = |p: &(f64, f64)| (p.0 - cursor.0).abs() <= 0.5 && (p.1 - cursor.1).abs() <= 0.5;
        match self.recent.back() {
            Some(&newest) if self.recent.iter().any(near) => newest,
            _ => {
                self.moved_to(cursor);
                cursor
            }
        }
    }

    fn moved_to(&mut self, at: (f64, f64)) {
        if self.recent.len() == RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(at);
    }

    /// A move to `at`: a drag of the lowest held button (left, then right, then the others),
    /// else a plain move.
    fn motion(&mut self, at: (f64, f64), delta: (i32, i32)) -> Post {
        self.moved_to(at);
        let held = (self.buttons != 0).then(|| self.buttons.trailing_zeros());
        let (ty, button, clicks) = match held {
            Some(b) => (button_types(b).2, b, self.clicks_of(b)),
            None => (ty::MOVED, 0, 0),
        };
        Post::Mouse {
            ty,
            button,
            at,
            delta,
            clicks,
            flags: self.flags(),
        }
    }

    /// The click state of `button`'s latest press.
    fn clicks_of(&self, button: u32) -> i64 {
        self.press
            .filter(|p| p.button == button)
            .map_or(1, |p| p.count)
    }

    /// A scroll plan as events. Point distance and wheel lines go out whole, the fraction kept
    /// per axis; a stop ends the gesture once no axis is left in it.
    fn lower(&mut self, ops: Vec<ScrollOp>, phased: bool) -> Vec<Post> {
        let flags = self.flags();
        let mut out = Vec::new();
        for op in ops {
            match op {
                ScrollOp::Continuous { horizontal, value } => {
                    let a = usize::from(horizontal);
                    let total = self.px[a] + value;
                    let whole = total.trunc();
                    self.px[a] = total - whole;
                    if whole == 0.0 {
                        continue;
                    }
                    let phase = match phased {
                        false => 0,
                        true if self.open == [false; 2] => phase::BEGAN,
                        true => phase::CHANGED,
                    };
                    self.open[a] |= phased;
                    out.push(scroll(false, horizontal, -(whole as i32), phase, flags));
                }
                ScrollOp::Discrete120 { horizontal, value } => {
                    let a = usize::from(horizontal);
                    if let Some((_, lines)) = self.wheel[a].turn(value.saturating_mul(256))
                        && lines != 0
                    {
                        out.push(scroll(true, horizontal, -lines, 0, flags));
                    }
                }
                ScrollOp::Stop { horizontal, cancel } => {
                    let a = usize::from(horizontal);
                    self.px[a] = 0.0;
                    if std::mem::take(&mut self.open[a]) && self.open == [false; 2] {
                        let end = if cancel {
                            phase::CANCELLED
                        } else {
                            phase::ENDED
                        };
                        out.push(scroll(false, horizontal, 0, end, flags));
                    }
                }
                // Each CGEvent is its own frame, and a libei plan names no source.
                ScrollOp::Frame | ScrollOp::AxisSource(_) | ScrollOp::DiscreteDetents { .. } => {}
            }
        }
        out
    }
}

/// A scroll on one axis. `value` is in the libei plan's sign (down and right positive), which
/// is the opposite of both macOS wheels.
fn scroll(lines: bool, horizontal: bool, value: i32, phase: u32, flags: u64) -> Post {
    let (wheel1, wheel2) = if horizontal { (0, value) } else { (value, 0) };
    Post::Scroll {
        lines,
        wheel1,
        wheel2,
        phase,
        flags,
    }
}

/// A down and an up for one committed scalar; nothing for a control character, which rides
/// the VK path (Enter, Backspace, Tab), or a code that is no scalar.
fn text(code: u32) -> Vec<Post> {
    char::from_u32(code)
        .filter(|c| !c.is_control())
        .map_or_else(Vec::new, |ch| {
            vec![
                Post::Text { ch, down: true },
                Post::Text { ch, down: false },
            ]
        })
}

/// `(down, up, dragged)` event types of a `CGMouseButton`.
fn button_types(button: u32) -> (u32, u32, u32) {
    match button {
        0 => (ty::LEFT_DOWN, ty::LEFT_UP, ty::LEFT_DRAGGED),
        1 => (ty::RIGHT_DOWN, ty::RIGHT_UP, ty::RIGHT_DRAGGED),
        _ => (ty::OTHER_DOWN, ty::OTHER_UP, ty::OTHER_DRAGGED),
    }
}

/// The stream display's bounds, or `main` when there is none or it is gone (an empty rect).
pub(crate) fn pick_bounds(stream: Option<Rect>, main: Rect) -> Rect {
    stream.filter(|r| r.2 >= 1.0 && r.3 >= 1.0).unwrap_or(main)
}

/// `p` moved onto `b`. The far edge, `x + w`, is the next display's first column.
fn clamp_to(b: Rect, (x, y): (f64, f64)) -> (f64, f64) {
    let last = |o: f64, n: f64| o + (n - 1.0).max(0.0);
    (x.clamp(b.0, last(b.0, b.2)), y.clamp(b.1, last(b.1, b.3)))
}

/// The flags a held modifier key sets: its mask and its side bit.
fn modifier(code: u16) -> Option<u64> {
    use flag::*;
    Some(match code {
        0x38 => SHIFT | L_SHIFT,
        0x3C => SHIFT | R_SHIFT,
        0x3B => CONTROL | L_CTRL,
        0x3E => CONTROL | R_CTRL,
        0x3A => ALTERNATE | L_ALT,
        0x3D => ALTERNATE | R_ALT,
        0x37 => COMMAND | L_CMD,
        0x36 => COMMAND | R_CMD,
        _ => return None,
    })
}

/// Wire VK (US-positional, as [`crate::vk_to_evdev`] reads it) → macOS key code (`kVK_*`,
/// ANSI-positional). A PC key a Mac keyboard lacks takes the code macOS gives a PC keyboard's:
/// Print Screen, Scroll Lock and Pause are F13–F15, Insert is Help, Num Lock is keypad Clear.
/// Media keys and the Japanese conversion keys have no key code.
pub(crate) fn mac_key(vk: u8) -> Option<u16> {
    Some(match vk {
        0x08 => 0x33,        // VK_BACK      -> kVK_Delete
        0x09 => 0x30,        // VK_TAB       -> kVK_Tab
        0x0D => 0x24,        // VK_RETURN    -> kVK_Return
        0x13 => 0x71,        // VK_PAUSE     -> kVK_F15
        0x14 => 0x39,        // VK_CAPITAL   -> kVK_CapsLock
        0x1B => 0x35,        // VK_ESCAPE    -> kVK_Escape
        0x20 => 0x31,        // VK_SPACE     -> kVK_Space
        0x21 => 0x74,        // VK_PRIOR     -> kVK_PageUp
        0x22 => 0x79,        // VK_NEXT      -> kVK_PageDown
        0x23 => 0x77,        // VK_END       -> kVK_End
        0x24 => 0x73,        // VK_HOME      -> kVK_Home
        0x25 => 0x7B,        // VK_LEFT      -> kVK_LeftArrow
        0x26 => 0x7E,        // VK_UP        -> kVK_UpArrow
        0x27 => 0x7C,        // VK_RIGHT     -> kVK_RightArrow
        0x28 => 0x7D,        // VK_DOWN      -> kVK_DownArrow
        0x2C => 0x69,        // VK_SNAPSHOT  -> kVK_F13
        0x2D => 0x72,        // VK_INSERT    -> kVK_Help
        0x2E => 0x75,        // VK_DELETE    -> kVK_ForwardDelete
        0x10 | 0xA0 => 0x38, // VK_SHIFT, VK_LSHIFT     -> kVK_Shift
        0x11 | 0xA2 => 0x3B, // VK_CONTROL, VK_LCONTROL -> kVK_Control
        0x12 | 0xA4 => 0x3A, // VK_MENU, VK_LMENU       -> kVK_Option
        0xA1 => 0x3C,        // VK_RSHIFT    -> kVK_RightShift
        0xA3 => 0x3E,        // VK_RCONTROL  -> kVK_RightControl
        0xA5 => 0x3D,        // VK_RMENU     -> kVK_RightOption
        0x5B => 0x37,        // VK_LWIN      -> kVK_Command
        0x5C => 0x36,        // VK_RWIN      -> kVK_RightCommand
        0x5D => 0x6E,        // VK_APPS      -> kVK_ContextualMenu

        // kVK_ANSI_0..9 are not sequential.
        0x30 => 0x1D,
        0x31 => 0x12,
        0x32 => 0x13,
        0x33 => 0x14,
        0x34 => 0x15,
        0x35 => 0x17,
        0x36 => 0x16,
        0x37 => 0x1A,
        0x38 => 0x1C,
        0x39 => 0x19,

        0x41 => 0x00, // A
        0x42 => 0x0B, // B
        0x43 => 0x08, // C
        0x44 => 0x02, // D
        0x45 => 0x0E, // E
        0x46 => 0x03, // F
        0x47 => 0x05, // G
        0x48 => 0x04, // H
        0x49 => 0x22, // I
        0x4A => 0x26, // J
        0x4B => 0x28, // K
        0x4C => 0x25, // L
        0x4D => 0x2E, // M
        0x4E => 0x2D, // N
        0x4F => 0x1F, // O
        0x50 => 0x23, // P
        0x51 => 0x0C, // Q
        0x52 => 0x0F, // R
        0x53 => 0x01, // S
        0x54 => 0x11, // T
        0x55 => 0x20, // U
        0x56 => 0x09, // V
        0x57 => 0x0D, // W
        0x58 => 0x07, // X
        0x59 => 0x10, // Y (US position: a QWERTZ host types Z)
        0x5A => 0x06, // Z (US position)

        0x60 => 0x52, // KP0
        0x61 => 0x53, // KP1
        0x62 => 0x54, // KP2
        0x63 => 0x55, // KP3
        0x64 => 0x56, // KP4
        0x65 => 0x57, // KP5
        0x66 => 0x58, // KP6
        0x67 => 0x59, // KP7
        0x68 => 0x5B, // KP8
        0x69 => 0x5C, // KP9
        0x6A => 0x43, // VK_MULTIPLY  -> kVK_ANSI_KeypadMultiply
        0x6B => 0x45, // VK_ADD       -> kVK_ANSI_KeypadPlus
        0x6C => 0x4C, // VK_SEPARATOR -> kVK_ANSI_KeypadEnter
        0x6D => 0x4E, // VK_SUBTRACT  -> kVK_ANSI_KeypadMinus
        0x6E => 0x41, // VK_DECIMAL   -> kVK_ANSI_KeypadDecimal
        0x6F => 0x4B, // VK_DIVIDE    -> kVK_ANSI_KeypadDivide

        0x70 => 0x7A, // F1
        0x71 => 0x78, // F2
        0x72 => 0x63, // F3
        0x73 => 0x76, // F4
        0x74 => 0x60, // F5
        0x75 => 0x61, // F6
        0x76 => 0x62, // F7
        0x77 => 0x64, // F8
        0x78 => 0x65, // F9
        0x79 => 0x6D, // F10
        0x7A => 0x67, // F11
        0x7B => 0x6F, // F12

        0x90 => 0x47, // VK_NUMLOCK -> kVK_ANSI_KeypadClear
        0x91 => 0x6B, // VK_SCROLL  -> kVK_F14

        0xBA => 0x29, // VK_OEM_1      -> kVK_ANSI_Semicolon
        0xBB => 0x18, // VK_OEM_PLUS   -> kVK_ANSI_Equal
        0xBC => 0x2B, // VK_OEM_COMMA  -> kVK_ANSI_Comma
        0xBD => 0x1B, // VK_OEM_MINUS  -> kVK_ANSI_Minus
        0xBE => 0x2F, // VK_OEM_PERIOD -> kVK_ANSI_Period
        0xBF => 0x2C, // VK_OEM_2      -> kVK_ANSI_Slash
        0xC0 => 0x32, // VK_OEM_3      -> kVK_ANSI_Grave
        0xDB => 0x21, // VK_OEM_4      -> kVK_ANSI_LeftBracket
        0xDC => 0x2A, // VK_OEM_5      -> kVK_ANSI_Backslash
        0xDD => 0x1E, // VK_OEM_6      -> kVK_ANSI_RightBracket
        0xDE => 0x27, // VK_OEM_7      -> kVK_ANSI_Quote
        0xE2 => 0x0A, // VK_OEM_102    -> kVK_ISO_Section (ISO key left of Z)

        // The Korean toggles are a PC keyboard's LANG1/LANG2, which macOS reads as the JIS
        // kana and eisu keys.
        0x15 => 0x68, // VK_HANGUL       -> kVK_JIS_Kana
        0x19 => 0x66, // VK_HANJA        -> kVK_JIS_Eisu
        0xF3 => 0x32, // VK_DBE_SBCSCHAR -> kVK_ANSI_Grave (半角/全角 sits there)
        0xC1 => 0x5E, // VK_ABNT_C1      -> kVK_JIS_Underscore (JIS ろ)
        0xC2 => 0x5F, // VK_ABNT_C2      -> kVK_JIS_KeypadComma
        0xE1 => 0x5D, // VK_OEM_AX       -> kVK_JIS_Yen

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::input::scroll::{ScrollSource, SCROLL_SCALE};

    const VK_A: u32 = 0x41;
    const VK_SHIFT: u32 = 0x10;
    const VK_LSHIFT: u32 = 0xA0;
    const VK_RSHIFT: u32 = 0xA1;
    const VK_LCONTROL: u32 = 0xA2;
    const VK_CAPITAL: u32 = 0x14;
    const KVK_A: u16 = 0x00;
    const KVK_SHIFT: u16 = 0x38;
    const KVK_RIGHT_SHIFT: u16 = 0x3C;
    const SHIFTED: u64 = flag::SHIFT | flag::L_SHIFT;

    /// A cursor left of the stream display, which sits right of a 1920-wide main one.
    const DESK: Desk = Desk {
        cursor: (100.0, 100.0),
        bounds: (1920.0, 0.0, 2560.0, 1440.0),
    };

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

    fn moved(dx: i32, dy: i32) -> InputEvent {
        InputEvent {
            x: dx,
            y: dy,
            ..ev(InputKind::MouseMove, 0)
        }
    }

    fn key(ty: u32, code: u16, flags: u64, repeat: bool) -> Post {
        Post::Key {
            ty,
            code,
            flags,
            repeat,
        }
    }

    fn planner() -> Planner {
        Planner::new(Duration::from_millis(500))
    }

    fn run(p: &mut Planner, events: &[InputEvent]) -> Vec<Post> {
        let now = Instant::now();
        events
            .iter()
            .flat_map(|e| p.plan(e, now, &|| DESK))
            .collect()
    }

    /// `(ty, button, clicks)` of each mouse event.
    fn mice(posts: &[Post]) -> Vec<(u32, u32, i64)> {
        (posts.iter())
            .filter_map(|p| match *p {
                Post::Mouse {
                    ty, button, clicks, ..
                } => Some((ty, button, clicks)),
                _ => None,
            })
            .collect()
    }

    /// The Mac table knows every key the Linux one does, but for keys a Mac has no code for.
    #[test]
    fn every_wire_key_reaches_a_mac_key_but_media_and_conversion() {
        let no_mac_key = [0xB0, 0xB1, 0xB2, 0xB3, 0x1C, 0x1D, 0xF2];
        for vk in 0..=255u8 {
            let want = crate::vk_to_evdev(vk).is_some() && !no_mac_key.contains(&vk);
            assert_eq!(mac_key(vk).is_some(), want, "vk {vk:#04x}");
        }
        assert_eq!(mac_key(0x59), Some(0x10), "Y stays on the US Y position");
        assert_eq!(mac_key(0x5A), Some(0x06), "Z stays on the US Z position");
        assert_eq!(mac_key(0xBA), Some(0x29), ";: is the QWERTZ ö position");
    }

    /// Two VKs share a Mac key only where one names the other: a generic modifier and its left
    /// side, and the JIS 半角/全角 on the grave position.
    #[test]
    fn no_two_wire_keys_share_a_mac_key_by_accident() {
        let shared = [(0x10, 0xA0), (0x11, 0xA2), (0x12, 0xA4), (0xC0, 0xF3)];
        let mut seen = std::collections::HashMap::new();
        for vk in 0..=255u8 {
            if let Some(first) = mac_key(vk).and_then(|code| seen.insert(code, vk)) {
                assert!(shared.contains(&(first, vk)), "{first:#04x} and {vk:#04x}");
            }
        }
    }

    #[test]
    fn modifiers_are_flag_changes_and_every_key_carries_them() {
        let mut p = planner();
        let posts = run(
            &mut p,
            &[
                down(VK_LSHIFT),
                down(VK_A),
                down(VK_A),
                up(VK_A),
                up(VK_LSHIFT),
            ],
        );
        assert_eq!(
            posts,
            [
                key(ty::FLAGS_CHANGED, KVK_SHIFT, SHIFTED, false),
                key(ty::KEY_DOWN, KVK_A, SHIFTED, false),
                key(ty::KEY_DOWN, KVK_A, SHIFTED, true),
                key(ty::KEY_UP, KVK_A, SHIFTED, false),
                key(ty::FLAGS_CHANGED, KVK_SHIFT, 0, false),
            ]
        );
    }

    /// The side bits tell an app which Shift moved while the other stays down.
    #[test]
    fn each_side_of_a_modifier_sets_its_own_bit() {
        let mut p = planner();
        let posts = run(&mut p, &[down(VK_LSHIFT), down(VK_RSHIFT), up(VK_RSHIFT)]);
        assert_eq!(
            posts[1..],
            [
                key(
                    ty::FLAGS_CHANGED,
                    KVK_RIGHT_SHIFT,
                    SHIFTED | flag::R_SHIFT,
                    false
                ),
                key(ty::FLAGS_CHANGED, KVK_RIGHT_SHIFT, SHIFTED, false),
            ]
        );
    }

    #[test]
    fn repeats_stale_ups_aliases_and_unmapped_keys_post_nothing() {
        let rows: [(&str, Vec<InputEvent>, usize); 4] = [
            ("a stale up", vec![up(VK_A)], 0),
            ("an unmapped key", vec![down(0xFF), up(0xFF)], 0),
            (
                "a modifier's repeat",
                vec![down(VK_LCONTROL), down(VK_LCONTROL)],
                1,
            ),
            (
                "a ring's Shift under a held Left Shift",
                vec![down(VK_LSHIFT), down(VK_SHIFT), up(VK_SHIFT), up(VK_LSHIFT)],
                2,
            ),
        ];
        for (name, events, n) in rows {
            assert_eq!(run(&mut planner(), &events).len(), n, "{name}");
        }
    }

    /// Caps Lock flips on a press and says nothing on its release; keys typed under it carry
    /// the lock.
    #[test]
    fn caps_lock_flips_on_each_press() {
        let mut p = planner();
        let posts = run(
            &mut p,
            &[
                down(VK_CAPITAL),
                up(VK_CAPITAL),
                down(VK_A),
                down(VK_CAPITAL),
                down(VK_CAPITAL),
                up(VK_CAPITAL),
            ],
        );
        assert_eq!(
            posts,
            [
                key(ty::FLAGS_CHANGED, KVK_CAPS_LOCK, flag::ALPHA_SHIFT, false),
                key(ty::KEY_DOWN, KVK_A, flag::ALPHA_SHIFT, false),
                key(ty::FLAGS_CHANGED, KVK_CAPS_LOCK, 0, false),
            ]
        );
    }

    #[test]
    fn text_is_a_down_and_an_up_per_scalar() {
        let posts = run(&mut planner(), &[ev(InputKind::TextInput, 0x1F600)]);
        let ch = '\u{1F600}';
        assert_eq!(
            posts,
            [
                Post::Text { ch, down: true },
                Post::Text { ch, down: false }
            ]
        );
        assert!(run(&mut planner(), &[ev(InputKind::TextInput, 0x0A)]).is_empty());
        assert!(run(&mut planner(), &[ev(InputKind::TextInput, 0xD800)]).is_empty());
    }

    /// A relative move lands on the stream display and keeps the client's own delta, which a
    /// game reads even with the cursor pinned at an edge.
    #[test]
    fn a_relative_move_stays_on_the_display_and_keeps_its_delta() {
        let posts = run(&mut planner(), &[moved(10, -500)]);
        let Post::Mouse { ty, at, delta, .. } = posts[0] else {
            panic!("{posts:?}")
        };
        assert_eq!((ty, at, delta), (ty::MOVED, (1920.0, 0.0), (10, -500)));
    }

    /// Moves posted faster than the window server applies them read back a cursor that lags:
    /// each still lands its full delta from the last one, and a click lands where they ended.
    /// A cursor this planner never put anywhere is someone else's move, and is followed.
    #[test]
    fn a_burst_read_back_stale_keeps_every_delta() {
        let mut p = planner();
        let now = Instant::now();
        let at = |p: &mut Planner, e: InputEvent, cursor: (f64, f64)| {
            let desk = Desk { cursor, ..DESK };
            match p.plan(&e, now, &|| desk)[..] {
                [Post::Mouse { at, delta, .. }] => (at, delta),
                ref other => panic!("{other:?}"),
            }
        };
        let start = (2000.0, 100.0);
        assert_eq!(at(&mut p, moved(10, 0), start), ((2010.0, 100.0), (10, 0)));
        assert_eq!(
            at(&mut p, moved(10, 5), start).0,
            (2020.0, 105.0),
            "stale read"
        );
        assert_eq!(
            at(&mut p, moved(10, 0), (2010.0, 100.0)).0,
            (2030.0, 105.0),
            "half caught up"
        );
        let click = at(&mut p, ev(InputKind::MouseButtonDown, 1), start);
        assert_eq!(
            click.0,
            (2030.0, 105.0),
            "a click lands where the moves ended"
        );
        assert_eq!(
            at(&mut p, moved(1, 1), (2500.0, 300.0)).0,
            (2501.0, 301.0),
            "followed"
        );
    }

    #[test]
    fn an_absolute_move_maps_into_the_display_bounds() {
        let abs = |x, y| InputEvent {
            x,
            y,
            flags: (1280 << 16) | 720,
            ..ev(InputKind::MouseMoveAbs, 0)
        };
        let at = |e| match run(&mut planner(), &[e])[..] {
            [Post::Mouse { at, delta, .. }] => (at, delta),
            ref other => panic!("{other:?}"),
        };
        assert_eq!(at(abs(640, 360)), ((3200.0, 720.0), (3100, 620)));
        let ((x, y), _) = at(abs(1280, 720));
        assert_eq!(
            (x, y),
            (4479.0, 1439.0),
            "the far edge stays on the display"
        );
        let zero = InputEvent {
            flags: 0,
            ..abs(1, 1)
        };
        assert!(run(&mut planner(), &[zero]).is_empty());
    }

    /// A drag is the lowest held button's: left, then right, then the others.
    #[test]
    fn a_move_with_a_button_held_is_that_buttons_drag() {
        let b = |kind, code| ev(kind, code);
        let (dn, upk) = (InputKind::MouseButtonDown, InputKind::MouseButtonUp);
        let posts = run(
            &mut planner(),
            &[
                b(dn, 1),
                moved(1, 0),
                b(dn, 3),
                moved(1, 0),
                b(upk, 1),
                moved(1, 0),
                b(upk, 3),
                moved(1, 0),
                b(dn, 2),
                moved(1, 0),
                b(upk, 2),
            ],
        );
        assert_eq!(
            mice(&posts),
            [
                (ty::LEFT_DOWN, 0, 1),
                (ty::LEFT_DRAGGED, 0, 1),
                (ty::RIGHT_DOWN, 1, 1),
                (ty::LEFT_DRAGGED, 0, 1),
                (ty::LEFT_UP, 0, 1),
                (ty::RIGHT_DRAGGED, 1, 1),
                (ty::RIGHT_UP, 1, 1),
                (ty::MOVED, 0, 0),
                (ty::OTHER_DOWN, 2, 1),
                (ty::OTHER_DRAGGED, 2, 1),
                (ty::OTHER_UP, 2, 1),
            ]
        );
    }

    #[test]
    fn every_wire_button_maps_and_a_repeat_or_stray_up_does_not() {
        let b = |kind, code| ev(kind, code);
        let (dn, upk) = (InputKind::MouseButtonDown, InputKind::MouseButtonUp);
        let posts = run(
            &mut planner(),
            &[
                b(dn, 4),
                b(dn, 4),
                b(upk, 4),
                b(upk, 4),
                b(dn, 5),
                b(dn, 6),
                b(dn, 0),
            ],
        );
        assert_eq!(
            mice(&posts),
            [
                (ty::OTHER_DOWN, 3, 1),
                (ty::OTHER_UP, 3, 1),
                (ty::OTHER_DOWN, 4, 1),
            ]
        );
        let Post::Mouse { at, .. } = posts[0] else {
            panic!()
        };
        assert_eq!(at, (1920.0, 100.0), "a press lands on the display too");
    }

    /// Presses of one button inside the interval and the slop count up, down and up alike.
    #[test]
    fn quick_presses_in_place_count_up() {
        let mut p = planner();
        let t0 = Instant::now();
        let mut click = |ms: u64, button: u32, y: f64| {
            let desk = Desk {
                cursor: (2000.0, y),
                ..DESK
            };
            let at = t0 + Duration::from_millis(ms);
            let mut posts = p.plan(&ev(InputKind::MouseButtonDown, button), at, &|| desk);
            posts.extend(p.plan(&ev(InputKind::MouseButtonUp, button), at, &|| desk));
            let clicks: Vec<i64> = mice(&posts).iter().map(|m| m.2).collect();
            clicks
        };
        assert_eq!(click(0, 1, 100.0), [1, 1]);
        assert_eq!(click(300, 1, 103.0), [2, 2], "a double-click");
        assert_eq!(click(600, 1, 103.0), [3, 3], "a triple-click");
        assert_eq!(click(1200, 1, 103.0), [1, 1], "too slow");
        assert_eq!(click(1300, 1, 120.0), [1, 1], "too far");
        assert_eq!(click(1400, 3, 120.0), [1, 1], "another button");
        assert_eq!(click(1500, 1, 120.0), [1, 1], "after another button");
    }

    fn scroll_ev(source: ScrollSource, phase: ScrollPhase, axis: u32, delta: f64) -> InputEvent {
        ScrollEvent {
            source,
            phase,
            axis,
            delta: (delta * SCROLL_SCALE) as i32,
            axis_units: false,
        }
        .to_event()
    }

    /// `(lines, wheel1, wheel2, phase)` of each scroll event.
    fn wheels(posts: &[Post]) -> Vec<(bool, i32, i32, u32)> {
        (posts.iter())
            .filter_map(|p| match *p {
                Post::Scroll {
                    lines,
                    wheel1,
                    wheel2,
                    phase,
                    ..
                } => Some((lines, wheel1, wheel2, phase)),
                _ => None,
            })
            .collect()
    }

    /// A wheel scrolls in whole lines, up and left positive; half detents add up.
    #[test]
    fn a_wheel_scrolls_in_lines() {
        use ScrollPhase::None as Plain;
        use ScrollSource::Wheel;
        let posts = run(
            &mut planner(),
            &[
                scroll_ev(Wheel, Plain, 0, 120.0),
                scroll_ev(Wheel, Plain, 1, 240.0),
                scroll_ev(Wheel, Plain, 0, -60.0),
                scroll_ev(Wheel, Plain, 0, -60.0),
            ],
        );
        assert_eq!(
            wheels(&posts),
            [(true, 1, 0, 0), (true, 0, -2, 0), (true, -1, 0, 0)]
        );
        let legacy = InputEvent {
            x: 120,
            ..ev(InputKind::MouseScroll, 0)
        };
        assert_eq!(wheels(&run(&mut planner(), &[legacy])), [(true, 1, 0, 0)]);
    }

    /// A finger gesture is one phased run of point deltas; its lift ends it once the momentum
    /// grace runs out, and the fraction of a point waits for the next delta.
    #[test]
    fn a_finger_gesture_scrolls_in_points_with_phases() {
        use ScrollPhase::*;
        use ScrollSource::Finger;
        let mut p = planner();
        let posts = run(
            &mut p,
            &[
                scroll_ev(Finger, Begin, 0, 10.0),
                scroll_ev(Finger, Update, 1, -10.0),
                scroll_ev(Finger, Update, 0, 2.5),
                scroll_ev(Finger, Update, 0, 0.5),
                scroll_ev(Finger, End, 1, 0.0),
                scroll_ev(Finger, End, 0, 0.0),
            ],
        );
        assert_eq!(
            wheels(&posts),
            [
                (false, 10, 0, phase::BEGAN),
                (false, 0, 10, phase::CHANGED),
                (false, 2, 0, phase::CHANGED),
                (false, 1, 0, phase::CHANGED),
            ]
        );
        let due = p.due().expect("the lift waits for momentum");
        assert!(p.flush(due - Duration::from_millis(1)).is_empty());
        assert_eq!(
            wheels(&p.flush(due + Duration::from_secs(1))),
            [(false, 0, 0, phase::ENDED)],
            "the gesture ends once both axes stopped"
        );
    }

    /// The client's momentum continues the gesture; its end ends it.
    #[test]
    fn momentum_continues_the_gesture() {
        use ScrollPhase::*;
        use ScrollSource::Finger;
        let posts = run(
            &mut planner(),
            &[
                scroll_ev(Finger, Begin, 0, 10.0),
                scroll_ev(Finger, End, 0, 0.0),
                scroll_ev(Finger, MomentumBegin, 0, 4.0),
                scroll_ev(Finger, MomentumEnd, 0, 0.0),
            ],
        );
        assert_eq!(
            wheels(&posts),
            [
                (false, 10, 0, phase::BEGAN),
                (false, 4, 0, phase::CHANGED),
                (false, 0, 0, phase::ENDED),
            ]
        );
    }

    /// A phaseless precise source carries no phase, so no gesture is left open.
    #[test]
    fn a_phaseless_precise_scroll_opens_no_gesture() {
        use ScrollSource::*;
        let posts = run(
            &mut planner(),
            &[
                scroll_ev(Continuous, ScrollPhase::None, 0, 3.0),
                scroll_ev(Wheel, ScrollPhase::None, 0, 120.0),
            ],
        );
        assert_eq!(wheels(&posts), [(false, 3, 0, 0), (true, 1, 0, 0)]);
    }

    /// What the injector still holds when it goes: the ups, then the gesture's cancel.
    #[test]
    fn release_lifts_what_is_held_and_cancels_the_gesture() {
        let mut p = planner();
        run(
            &mut p,
            &[
                down(VK_LSHIFT),
                down(VK_A),
                ev(InputKind::MouseButtonDown, 1),
                scroll_ev(ScrollSource::Finger, ScrollPhase::Begin, 0, 10.0),
            ],
        );
        let posts = p.release(Instant::now(), &|| DESK);
        let mut keys: Vec<(u32, u16)> = (posts.iter())
            .filter_map(|p| match *p {
                Post::Key { ty, code, .. } => Some((ty, code)),
                _ => None,
            })
            .collect();
        keys.sort();
        assert_eq!(keys, [(ty::KEY_UP, KVK_A), (ty::FLAGS_CHANGED, KVK_SHIFT)]);
        assert_eq!(mice(&posts), [(ty::LEFT_UP, 0, 1)]);
        assert_eq!(wheels(&posts), [(false, 0, 0, phase::CANCELLED)]);
        assert!(
            p.release(Instant::now(), &|| DESK).is_empty(),
            "nothing twice"
        );
    }

    #[test]
    fn a_missing_stream_display_falls_back_to_the_main_one() {
        let main = (0.0, 0.0, 1512.0, 982.0);
        let ext = (1512.0, -200.0, 2560.0, 1440.0);
        assert_eq!(pick_bounds(Some(ext), main), ext);
        assert_eq!(pick_bounds(None, main), main);
        assert_eq!(
            pick_bounds(Some((0.0, 0.0, 0.0, 0.0)), main),
            main,
            "a gone display"
        );
    }
}
