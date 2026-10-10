//! Presenter input: SDL events → capture state and wire `InputEvent`s.
//!
//! Capture is reversible. Engage at stream start and on click-into-video (that
//! click is not forwarded). Release on Ctrl+Alt+Shift+Q or focus loss; held
//! keys/buttons flush as ups. While captured, SDL relative mouse mode hides,
//! confines, and feeds raw deltas as `MouseMove`. Focus-loss re-engages on
//! gain; the chord stays released until the user opts in. A pointer the compositor
//! warps (the Steam Frame's laser) is placed, not summed, when the host takes
//! absolute input.
//!
//! Keys are SDL scancodes → VK via `keymap_sdl` (layout-independent). Relative
//! motion coalesces to one summed `MouseMove` per loop — a 1000 Hz mouse would
//! otherwise send a datagram per event.
//!
//! Desktop mode (`design/remote-desktop-sweep.md`) reuses engage/release but
//! never locks: the local cursor moves freely (hidden over the window) and
//! motion is latest-wins `MouseMoveAbs` through the letterbox. Gamescope EIS
//! is relative-only; those sessions pin to capture ([`Capture::new`] `abs_ok`).

use crate::keymap_sdl;
use crate::touch::{Abs, Act, Gestures, TouchScrollPhase};
use pf_client_core::trust::{MouseMode, TouchMode};
use punktfunk_core::client::NativeClient;
use punktfunk_core::input::scroll::{ScrollAccumulator, ScrollEvent, ScrollPhase, ScrollSource};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::quic::{GRANT_KEYBOARD, GRANT_POINTER};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

impl Act {
    /// Motion/scroll packing for the wire. Lives here, not in `touch.rs`: the
    /// gesture engine must not take the platform-gated core dependency.
    pub fn wire(self) -> Option<(InputKind, u32, i32, i32, u32)> {
        match self {
            Act::MoveRel { dx, dy } => Some((InputKind::MouseMove, 0, dx, dy, 0)),
            Act::MoveAbs(a) => Some((
                InputKind::MouseMoveAbs,
                0,
                a.x,
                a.y,
                ((a.w & 0xffff) << 16) | (a.h & 0xffff),
            )),
            Act::Scroll { axis, delta, phase } => {
                let phase = match phase {
                    TouchScrollPhase::Begin => ScrollPhase::Begin,
                    TouchScrollPhase::Update => ScrollPhase::Update,
                    TouchScrollPhase::End => ScrollPhase::End,
                    TouchScrollPhase::Cancel => ScrollPhase::Cancel,
                };
                let ev = ScrollEvent {
                    source: ScrollSource::Touch,
                    phase,
                    axis,
                    delta,
                }
                .to_event();
                Some((ev.kind, ev.code, ev.x, ev.y, ev.flags))
            }
            Act::Button { .. }
            | Act::CycleStats
            | Act::Dial { .. }
            | Act::DialCommit
            | Act::DialCancel => None,
        }
    }
}

/// One SDL finger event. SDL never batches fingers in a single event.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FingerPhase {
    Down,
    Move,
    Up,
}

pub struct Capture {
    connector: Arc<NativeClient>,
    captured: bool,
    /// Chord release: focus-gain must not re-engage.
    user_released: bool,
    held_keys: HashSet<u8>,
    held_buttons: HashSet<u32>,
    /// Relative motion not yet on the wire, summed per loop iteration. Fractional: a slow
    /// touchpad moves less than a pixel per event, and the remainder carries to the next send.
    pending_rel: (f32, f32),
    /// Desktop-model position not yet on the wire, latest-wins per loop iteration.
    pending_abs: Option<Abs>,
    /// See [`Self::last_abs`].
    last_abs: Option<(i32, i32)>,
    /// Never true unless `abs_ok`.
    desktop: bool,
    /// Host injector accepts `MouseMoveAbs` (any compositor but gamescope).
    abs_ok: bool,
    /// SDL wheel quantization — SDL reports detents with no source or phase, so
    /// the fallback rides as `Unknown` v120.
    scroll_acc: ScrollAccumulator,
    /// Scroll axes a native capture path (`wayland_scroll`) has open, so a
    /// capture release can cancel them.
    scroll_axes: [Option<ScrollSource>; 2],
    /// SDL finger id → compact host slot (`TouchDown`). SDL ids are opaque and
    /// large; slots reuse after up, flush on release. [`TouchMode::Touch`] only.
    touch_slots: HashMap<u64, u32>,
    touch_mode: TouchMode,
    gestures: Gestures,
    /// Session access mask: the capture gate. Core drops refused events on send.
    /// Live via [`Capture::set_grants`].
    grants: u32,
}

fn send(connector: &NativeClient, kind: InputKind, code: u32, x: i32, y: i32, flags: u32) {
    let _ = connector.send_input(&InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags,
    });
}

impl Capture {
    /// Without `abs_ok` the desktop model is unavailable and `mouse_mode`
    /// silently resolves to capture. `invert_scroll` seeds the live core
    /// setting; [`NativeClient::set_invert_scroll`] changes it later.
    pub fn new(
        connector: Arc<NativeClient>,
        touch_mode: TouchMode,
        invert_scroll: bool,
        mouse_mode: MouseMode,
        abs_ok: bool,
        grants: u32,
    ) -> Capture {
        connector.set_invert_scroll(invert_scroll);
        Capture {
            connector,
            captured: false,
            user_released: false,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            pending_rel: (0.0, 0.0),
            pending_abs: None,
            last_abs: None,
            desktop: abs_ok && mouse_mode == MouseMode::Desktop,
            abs_ok,
            scroll_acc: ScrollAccumulator::new(),
            scroll_axes: [None; 2],
            touch_slots: HashMap::new(),
            touch_mode,
            gestures: Gestures::new(touch_mode != TouchMode::Pointer),
            grants,
        }
    }

    /// Physical px per DIP for gesture ballistics — the window's display scale.
    pub fn set_touch_density(&mut self, pixels_per_dip: f32) {
        self.gestures.set_density(pixels_per_dip);
    }

    pub fn captured(&self) -> bool {
        self.captured
    }

    /// Access mask the run loop feeds `apply_capture` so lock/grab track it.
    pub fn grants(&self) -> u32 {
        self.grants
    }

    /// Without POINTER or KEYBOARD, engage would lock a pointer that lands nowhere.
    pub fn can_capture(&self) -> bool {
        self.grants & (GRANT_POINTER | GRANT_KEYBOARD) != 0
    }

    /// Mid-session `AccessUpdate`. Lost classes drop their held state. The run
    /// loop then re-applies lock/grab, and releases capture if
    /// [`Capture::can_capture`] went false.
    pub fn set_grants(&mut self, grants: u32) {
        if grants == self.grants {
            return;
        }
        let lost = self.grants & !grants;
        if lost & GRANT_KEYBOARD != 0 {
            self.release_keys();
        }
        if lost & GRANT_POINTER != 0 {
            self.pending_rel = (0.0, 0.0);
            self.pending_abs = None;
            self.release_contacts();
            self.reset_touch_gestures();
            self.cancel_scroll_axes();
        }
        self.grants = grants;
    }

    pub fn desktop(&self) -> bool {
        self.desktop
    }

    /// A mouse button is down on the host: a model flip now would cut the drag.
    pub fn buttons_held(&self) -> bool {
        !self.held_buttons.is_empty()
    }

    /// Ctrl+Alt+Shift+M. `None` if the host cannot take absolute events
    /// (gamescope). Pending motion from the old model is dropped, not sent.
    pub fn toggle_desktop(&mut self) -> Option<bool> {
        if !self.abs_ok {
            return None;
        }
        self.desktop = !self.desktop;
        self.pending_rel = (0.0, 0.0);
        self.pending_abs = None;
        Some(self.desktop)
    }

    /// Host-driven flip: `relative_hint` grabbed/hid the pointer → relative;
    /// hint clear → absolute. Same gating and pending-motion drop as
    /// [`toggle_desktop`](Self::toggle_desktop).
    pub fn set_desktop(&mut self, on: bool) -> bool {
        if !self.abs_ok || self.desktop == on {
            return false;
        }
        self.desktop = on;
        self.pending_rel = (0.0, 0.0);
        self.pending_abs = None;
        true
    }

    /// Re-engage on focus gain unless the user released via the chord.
    pub fn should_reengage(&self) -> bool {
        !self.captured && !self.user_released
    }

    /// Caller turns on SDL relative mouse only if this returns true: otherwise
    /// grants cover neither pointer nor keyboard ([`Capture::can_capture`]).
    pub fn engage(&mut self) -> bool {
        if !self.can_capture() {
            return false;
        }
        self.user_released = false;
        self.captured = true;
        true
    }

    /// Send an up for everything currently held, and forget it. Capture is untouched: an
    /// overlay that starts eating events needs this WITHOUT releasing the pointer, and a
    /// press whose release never reaches the host stays down there forever.
    pub fn flush_held(&mut self) {
        self.release_keys();
        self.release_contacts();
        self.reset_touch_gestures();
        self.cancel_scroll_axes();
    }

    fn release_keys(&mut self) {
        for vk in self.held_keys.drain() {
            send(&self.connector, InputKind::KeyUp, vk as u32, 0, 0, 0);
        }
    }

    fn release_contacts(&mut self) {
        for button in self.held_buttons.drain() {
            send(&self.connector, InputKind::MouseButtonUp, button, 0, 0, 0);
        }
        for slot in self.touch_slots.drain().map(|(_, slot)| slot) {
            send(&self.connector, InputKind::TouchUp, slot, 0, 0, 0);
        }
    }

    fn reset_touch_gestures(&mut self) {
        // Held buttons release separately; reset emits only the gesture's owed scroll cancels.
        for act in self.gestures.reset() {
            self.apply_touch_act(act);
        }
    }

    /// Close scroll axes a native capture path left open. Runs before the local
    /// state drops so the host's interaction never dangles.
    fn cancel_scroll_axes(&mut self) {
        self.scroll_acc = ScrollAccumulator::new();
        for (axis, source) in self.scroll_axes.iter_mut().enumerate() {
            if let Some(source) = source.take() {
                let ev = ScrollEvent {
                    source,
                    phase: ScrollPhase::Cancel,
                    axis: axis as u32,
                    delta: 0,
                }
                .to_event();
                send(&self.connector, ev.kind, ev.code, ev.x, ev.y, ev.flags);
            }
        }
    }

    /// Flush held keys/buttons/touches as ups. `by_user` (the chord) stays
    /// released; focus loss re-engages on gain. Caller turns off relative mouse.
    pub fn release(&mut self, by_user: bool) -> bool {
        if by_user {
            self.user_released = true;
        }
        if !std::mem::replace(&mut self.captured, false) {
            return false;
        }
        self.pending_rel = (0.0, 0.0); // never send motion gathered while captured
        self.pending_abs = None;
        self.flush_held();
        true
    }

    /// One datagram per loop. Only one store is populated; the run loop routes
    /// by [`desktop`](Self::desktop). Relative motion goes out in whole pixels.
    pub fn flush_motion(&mut self) {
        let (dx, dy) = (self.pending_rel.0.trunc(), self.pending_rel.1.trunc());
        self.pending_rel.0 -= dx;
        self.pending_rel.1 -= dy;
        if dx != 0.0 || dy != 0.0 {
            send(
                &self.connector,
                InputKind::MouseMove,
                0,
                dx as i32,
                dy as i32,
                0,
            );
        }
        if let Some(a) = self.pending_abs.take() {
            send(
                &self.connector,
                InputKind::MouseMoveAbs,
                0,
                a.x,
                a.y,
                Self::touch_flags(a.w, a.h),
            );
        }
    }

    pub fn on_motion(&mut self, xrel: f32, yrel: f32) {
        if self.captured && !self.desktop {
            self.pending_rel.0 += xrel;
            self.pending_rel.1 += yrel;
        }
    }

    /// Frame position under the video fit: the desktop model's motion, or a warped
    /// pointer under capture. Latest-wins: intermediates add nothing (deltas must sum).
    pub fn on_motion_abs(&mut self, abs: Abs) {
        if self.captured && self.abs_ok {
            self.last_abs = Some((abs.x, abs.y));
            self.pending_abs = Some(abs);
        }
    }

    /// The host takes `MouseMoveAbs`, so a warped pointer can be placed under capture.
    pub fn abs_ok(&self) -> bool {
        self.abs_ok
    }

    /// Where this client last put the host pointer, in frame pixels; `None` before any
    /// absolute motion. The local cursor follows the host from here when they differ.
    pub fn last_abs(&self) -> Option<(i32, i32)> {
        self.last_abs
    }

    /// The local cursor was moved to the host's `pos`: that is now where we put it.
    pub fn followed_host(&mut self, pos: (i32, i32)) {
        self.last_abs = Some(pos);
    }

    pub fn on_key_down(&mut self, sc: sdl3::keyboard::Scancode) {
        if !self.captured {
            return;
        }
        if let Some(vk) = keymap_sdl::scancode_to_vk(sc) {
            // Host must see the cursor where the user does when the key lands.
            self.flush_motion();
            self.held_keys.insert(vk);
            send(&self.connector, InputKind::KeyDown, vk as u32, 0, 0, 0);
        }
    }

    /// OS auto-repeat of a held key. Only a key whose press reached the host repeats; a
    /// chord or toggle swallowed its press, so its repeats stay here too.
    pub fn on_key_repeat(&mut self, sc: sdl3::keyboard::Scancode) {
        if keymap_sdl::scancode_to_vk(sc).is_some_and(|vk| self.held_keys.contains(&vk)) {
            self.on_key_down(sc);
        }
    }

    pub fn on_key_up(&mut self, sc: sdl3::keyboard::Scancode) {
        if let Some(vk) = keymap_sdl::scancode_to_vk(sc) {
            // Flush-on-release may have already sent this up.
            if self.held_keys.remove(&vk) {
                send(&self.connector, InputKind::KeyUp, vk as u32, 0, 0, 0);
            }
        }
    }

    /// The engaging click never reaches here. Flush motion first so the down
    /// lands where the host cursor is.
    pub fn on_button_down(&mut self, b: sdl3::mouse::MouseButton) {
        if !self.captured {
            return;
        }
        self.flush_motion();
        if let Some(gs) = keymap_sdl::mouse_button_to_gs(b) {
            self.held_buttons.insert(gs);
            send(&self.connector, InputKind::MouseButtonDown, gs, 0, 0, 0);
        }
    }

    pub fn on_button_up(&mut self, b: sdl3::mouse::MouseButton) {
        self.flush_motion(); // the release must not beat the motion before it
        if let Some(gs) = keymap_sdl::mouse_button_to_gs(b) {
            if self.held_buttons.remove(&gs) {
                send(&self.connector, InputKind::MouseButtonUp, gs, 0, 0, 0);
            }
        }
    }

    /// SDL wheel fallback: detents with no source or phase, so `Unknown` v120 —
    /// the wire rule gives the host one honest reading. The fractional tail
    /// rides the accumulator; truncating each event would drop it. On Wayland
    /// the native pointer path supersedes this — `run` stops routing wheel
    /// events here once `WaylandScroll` is live.
    pub fn on_wheel(&mut self, dx: f32, dy: f32) {
        if !self.captured {
            return;
        }
        self.flush_motion(); // scroll happens at the latest cursor position
        for ev in crate::scroll::sdl_wheel(&mut self.scroll_acc, dx, dy) {
            send(&self.connector, ev.kind, ev.code, ev.x, ev.y, ev.flags);
        }
    }

    /// An already-normalized scroll event from a native capture path
    /// (`wayland_scroll`). Dropped while uncaptured — nothing replays on
    /// re-engage. Open axes are tracked so `release`/`flush_held` can close them.
    pub fn on_scroll(&mut self, ev: InputEvent) {
        let Some(se) = ScrollEvent::from_event(&ev) else {
            return;
        };
        if !self.captured {
            return;
        }
        self.flush_motion();
        self.scroll_axes[se.axis as usize] = if se.is_stop() || se.is_wheel() {
            None
        } else {
            Some(se.source)
        };
        send(&self.connector, ev.kind, ev.code, ev.x, ev.y, ev.flags);
    }

    fn touch_slot(&mut self, finger_id: u64) -> u32 {
        if let Some(&slot) = self.touch_slots.get(&finger_id) {
            return slot;
        }
        let used: HashSet<u32> = self.touch_slots.values().copied().collect();
        let slot = (0u32..).find(|s| !used.contains(s)).unwrap_or(0);
        self.touch_slots.insert(finger_id, slot);
        slot
    }

    /// Pack client surface size so the host can rescale. Same layout as
    /// Android `nativeSendTouch`.
    fn touch_flags(w: u32, h: u32) -> u32 {
        ((w & 0xffff) << 16) | (h & 0xffff)
    }

    /// `x`/`y` are absolute in the `w`×`h` content surface, not window pixels.
    /// Ignored unless captured — the overlay is gamepad-driven.
    pub fn on_touch_down(&mut self, finger_id: u64, x: i32, y: i32, w: u32, h: u32) {
        if !self.captured {
            return;
        }
        let slot = self.touch_slot(finger_id);
        send(
            &self.connector,
            InputKind::TouchDown,
            slot,
            x,
            y,
            Self::touch_flags(w, h),
        );
    }

    /// Skip a finger with no slot: capture engaged mid-touch has no host contact.
    pub fn on_touch_move(&mut self, finger_id: u64, x: i32, y: i32, w: u32, h: u32) {
        if !self.captured {
            return;
        }
        if let Some(&slot) = self.touch_slots.get(&finger_id) {
            send(
                &self.connector,
                InputKind::TouchMove,
                slot,
                x,
                y,
                Self::touch_flags(w, h),
            );
        }
    }

    /// Always run, even uncaptured: `release()` may have flushed the slot, but
    /// a stray up must not leave a pressed contact on the host.
    pub fn on_touch_up(&mut self, finger_id: u64) {
        if let Some(slot) = self.touch_slots.remove(&finger_id) {
            send(&self.connector, InputKind::TouchUp, slot, 0, 0, 0);
        }
    }

    /// `wx`/`wy` are physical window pixels (trackpad ballistics); `abs` is the
    /// frame position under the video fit (pointer / passthrough). `Touch` goes on the
    /// wire; `Trackpad`/`Pointer`/`Off` drive the gesture engine. Returns run-loop
    /// intents (`CycleStats`, dial); everything else is sent here, or dropped under `Off`.
    pub fn dispatch_finger(
        &mut self,
        phase: FingerPhase,
        id: u64,
        wx: f32,
        wy: f32,
        abs: Abs,
        t_ms: f64,
    ) -> Vec<Act> {
        match self.touch_mode {
            TouchMode::Touch => {
                match phase {
                    FingerPhase::Down => self.on_touch_down(id, abs.x, abs.y, abs.w, abs.h),
                    FingerPhase::Move => self.on_touch_move(id, abs.x, abs.y, abs.w, abs.h),
                    FingerPhase::Up => self.on_touch_up(id),
                }
                Vec::new()
            }
            TouchMode::Trackpad | TouchMode::Pointer | TouchMode::Off => {
                // Down/Move only while captured. Up always runs so a lift can
                // finish a gesture after focus-loss mid-touch.
                if !self.captured && phase != FingerPhase::Up {
                    return Vec::new();
                }
                let acts = match phase {
                    FingerPhase::Down => self.gestures.down(id, wx, wy, abs, t_ms),
                    FingerPhase::Move => self.gestures.motion(id, wx, wy, abs, t_ms),
                    FingerPhase::Up => self.gestures.up(id, t_ms),
                };
                acts.into_iter()
                    .filter_map(|act| self.apply_touch_act(act))
                    .collect()
            }
        }
    }

    pub fn touch_mode(&self) -> TouchMode {
        self.touch_mode
    }

    /// Mid-stream model switch. Flush held buttons/touches first — a drag
    /// must not survive — then restart the gesture engine.
    pub fn set_touch_mode(&mut self, mode: TouchMode) {
        self.release_contacts();
        self.reset_touch_gestures();
        self.touch_mode = mode;
        self.gestures = Gestures::new(mode != TouchMode::Pointer);
    }

    /// Down in order, up in reverse so modifiers stay held until the last key.
    pub fn send_chord(&mut self, vks: &[u8]) {
        for &vk in vks {
            send(&self.connector, InputKind::KeyDown, u32::from(vk), 0, 0, 0);
        }
        for &vk in vks.iter().rev() {
            send(&self.connector, InputKind::KeyUp, u32::from(vk), 0, 0, 0);
        }
    }

    /// Long-press arm, once per run-loop tick. `t_ms` is SDL ticks.
    /// [`TouchMode::Touch`] has no timer.
    pub fn tick(&mut self, t_ms: f64) {
        if !self.captured || self.touch_mode == TouchMode::Touch {
            return;
        }
        for act in self.gestures.tick(t_ms) {
            self.apply_touch_act(act);
        }
    }

    /// Track button holds in `held_buttons` so capture release flushes a
    /// tap-drag. Returns [`Act::CycleStats`] and dial intents to the run loop;
    /// under [`TouchMode::Off`] every other act is dropped.
    fn apply_touch_act(&mut self, act: Act) -> Option<Act> {
        match act {
            Act::CycleStats | Act::Dial { .. } | Act::DialCommit | Act::DialCancel => {
                return Some(act)
            }
            _ if self.touch_mode == TouchMode::Off => {}
            Act::Button { gs, down } => {
                if down {
                    self.flush_motion(); // the press lands where the cursor now is
                    self.held_buttons.insert(gs);
                    send(&self.connector, InputKind::MouseButtonDown, gs, 0, 0, 0);
                } else if self.held_buttons.remove(&gs) {
                    self.flush_motion();
                    send(&self.connector, InputKind::MouseButtonUp, gs, 0, 0, 0);
                }
            }
            other => {
                if let Some((kind, code, x, y, flags)) = other.wire() {
                    send(&self.connector, kind, code, x, y, flags);
                }
            }
        }
        None
    }
}
