//! The macOS host's virtual HID mouse and keyboard, behind `PUNKTFUNK_HID_INPUT=1`: one
//! `IOHIDUserDevice` with [`pf_driver_proto::mouse`]'s relative mouse and keyboard collections.
//! GameController and raw HID readers see it as hardware; a posted CGEvent reaches neither.
//! Off, or refused for want of the entitlement, every event stays on CGEvent.
//!
//! [`Router`] gives the device what a plugged-in mouse and keyboard would say:
//! - Moves that carry a mouse's own counts ([`InputEvent::mouse_counts_time`]). macOS
//!   accelerates them by the host's mouse settings, once. A finished pointer distance and an
//!   absolute position stay on CGEvent, which moves the cursor by exactly that much.
//! - Buttons and wheel turns while the device moved the pointer last. A drag then stays on the
//!   one path that knows the button is down; a release goes where its press went.
//! - Every key with a HID usage. macOS repeats a held key itself, so a repeated press sends
//!   nothing. Text, touch, finger scroll and unmapped keys stay on CGEvent.

use crate::plan::{mouse_op, MouseOp};
use crate::scroll::WheelAxis;
use pf_driver_proto::mouse::{
    keyboard_report, relative_moves, relative_report, set_key, wheel_turns, KEYBOARD_BITMAP_LEN,
};
use punktfunk_core::input::{InputEvent, InputKind};

/// `BTN_LEFT`: [`mouse_op`]'s buttons count up from it in HID order (primary, secondary,
/// middle, X1, X2).
const BTN_LEFT: u16 = 0x110;

/// What the device holds, and the reports each wire event makes.
#[derive(Default)]
pub(crate) struct Router {
    /// The device moved the pointer last: buttons and wheel turns go with it.
    pointer: bool,
    /// Wire buttons the device holds, by wire bit, as [`mouse_op`] keeps them.
    wire: u8,
    /// The same buttons by HID bit.
    buttons: u8,
    keys: [u8; KEYBOARD_BITMAP_LEN],
    /// Vertical, horizontal. macOS reads one wheel count as one detent.
    wheel: [WheelAxis; 2],
}

impl Router {
    /// The reports `ev` makes, `None` to leave it to CGEvent. Empty for an event the device
    /// takes that changes nothing it reports: a repeated key, a turn short of a detent.
    pub fn route(&mut self, ev: &InputEvent) -> Option<Vec<Vec<u8>>> {
        match ev.kind {
            InputKind::KeyDown | InputKind::KeyUp => return self.key(ev),
            InputKind::MouseMove => self.pointer = ev.mouse_counts_time().is_some(),
            InputKind::MouseMoveAbs => self.pointer = false,
            _ => {}
        }
        let reports = match mouse_op(ev, self.pointer, &mut self.wire)? {
            MouseOp::Motion { dx, dy, .. } => relative_moves(self.buttons, dx, dy).collect(),
            MouseOp::Button { evdev, down } => {
                let bit = 1u8 << (evdev - BTN_LEFT);
                let held = if down {
                    self.buttons | bit
                } else {
                    self.buttons & !bit
                };
                if held == self.buttons {
                    Vec::new()
                } else {
                    self.buttons = held;
                    vec![relative_report(held, 0, 0, 0, 0)]
                }
            }
            MouseOp::Wheel { horizontal, q8 } => {
                let detents = self.wheel[usize::from(horizontal)]
                    .turn(q8)
                    .map_or(0, |(_, d)| d);
                wheel_turns(self.buttons, horizontal, detents.into()).collect()
            }
        };
        Some(reports.iter().map(|r| r.to_vec()).collect())
    }

    /// `None` for a key with no usage, and for a release of one the device never pressed.
    fn key(&mut self, ev: &InputEvent) -> Option<Vec<Vec<u8>>> {
        let vk = (ev.code & 0xff) as u8;
        let usage = crate::keymap::vk_to_evdev(vk).and_then(crate::keymap::evdev_to_hid_usage)?;
        let down = ev.kind == InputKind::KeyDown;
        let changed = set_key(&mut self.keys, usage, down);
        if !changed && !down {
            return None;
        }
        Some(if changed {
            vec![keyboard_report(&self.keys).to_vec()]
        } else {
            Vec::new()
        })
    }

    /// The reports that let go of every button and key held; empty when nothing is.
    pub fn release(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if self.buttons != 0 {
            (self.buttons, self.wire) = (0, 0);
            out.push(relative_report(0, 0, 0, 0, 0).to_vec());
        }
        if self.keys != [0; KEYBOARD_BITMAP_LEN] {
            self.keys = [0; KEYBOARD_BITMAP_LEN];
            out.push(keyboard_report(&self.keys).to_vec());
        }
        out
    }
}

#[cfg(target_os = "macos")]
pub(crate) use device::HidInput;

#[cfg(target_os = "macos")]
mod device {
    use super::Router;
    use crate::hid_user_device::{HidUserDevice, Identity, CREATE_HINT};
    use pf_driver_proto::mouse::{
        rel_keyboard_rdesc, MOUSE_PID, MOUSE_REL_REPORT_ID, MOUSE_VER, MOUSE_VID,
    };
    use punktfunk_core::input::InputEvent;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// `1` puts mouse and keyboard on the virtual HID device. Read when the injector opens.
    const HID_INPUT_ENV: &str = "PUNKTFUNK_HID_INPUT";

    /// The device and what it holds. Dropping it lets go of both, then removes the device.
    pub(crate) struct HidInput {
        dev: HidUserDevice,
        router: Router,
    }

    impl HidInput {
        /// The device when [`HID_INPUT_ENV`] asks for it, else `None`. A refused create warns
        /// once per process.
        pub fn open() -> Option<HidInput> {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if pf_host_config::env_on(HID_INPUT_ENV) != Some(true) {
                return None;
            }
            let id = Identity {
                vendor: MOUSE_VID.into(),
                product: MOUSE_PID.into(),
                version: MOUSE_VER.into(),
                name: "Punktfunk Virtual Mouse",
                manufacturer: "Punktfunk",
                serial: "punktfunk-mouse".into(),
                rdesc: rel_keyboard_rdesc(),
                bluetooth: false,
                // The wheel multipliers off: one count is one detent.
                features: vec![vec![MOUSE_REL_REPORT_ID, 0]],
            };
            match HidUserDevice::open(&id) {
                Ok(dev) => {
                    tracing::info!("input: mouse and keyboard go through the virtual HID device");
                    Some(HidInput {
                        dev,
                        router: Router::default(),
                    })
                }
                Err(e) => {
                    if !WARNED.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            error = %format!("{e:#}{CREATE_HINT}"),
                            "virtual HID mouse and keyboard not created; input stays on CGEvent"
                        );
                    }
                    None
                }
            }
        }

        /// Send `ev` as reports. `false` leaves it to CGEvent, also when the device is gone.
        pub fn take(&mut self, ev: &InputEvent) -> bool {
            let Some(reports) = self.router.route(ev) else {
                return false;
            };
            for report in &reports {
                if let Err(e) = self.dev.write_input(report) {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "virtual HID device gone; input stays on CGEvent"
                    );
                    return false;
                }
            }
            true
        }

        /// `false` once the system refused a report.
        pub fn alive(&self) -> bool {
            self.dev.alive()
        }
    }

    impl Drop for HidInput {
        fn drop(&mut self) {
            for report in self.router.release() {
                let _ = self.dev.write_input(&report);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_driver_proto::mouse::{KEYBOARD_REPORT_ID, MOUSE_REL_REPORT_ID};
    use punktfunk_core::input::mouse_counts_flags;
    use punktfunk_core::input::scroll::{ScrollEvent, ScrollPhase, ScrollSource, SCROLL_SCALE};

    fn ev(kind: InputKind, code: u32, x: i32, y: i32, flags: u32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x,
            y,
            flags,
        }
    }

    fn counts(dx: i32, dy: i32) -> InputEvent {
        ev(InputKind::MouseMove, 0, dx, dy, mouse_counts_flags(1_000))
    }

    fn pixels(dx: i32, dy: i32) -> InputEvent {
        ev(InputKind::MouseMove, 0, dx, dy, 0)
    }

    fn button(code: u32, down: bool) -> InputEvent {
        let kind = if down {
            InputKind::MouseButtonDown
        } else {
            InputKind::MouseButtonUp
        };
        ev(kind, code, 0, 0, 0)
    }

    fn key(vk: u32, down: bool) -> InputEvent {
        let kind = if down {
            InputKind::KeyDown
        } else {
            InputKind::KeyUp
        };
        ev(kind, vk, 0, 0, 0)
    }

    fn wheel(source: ScrollSource, axis: u32, v120: f64) -> InputEvent {
        ScrollEvent {
            source,
            phase: ScrollPhase::None,
            axis,
            delta: (v120 * SCROLL_SCALE) as i32,
            axis_units: false,
        }
        .to_event()
    }

    fn rel(buttons: u8, dx: i16, dy: i16, wheel: i16, pan: i16) -> Vec<u8> {
        relative_report(buttons, dx, dy, wheel, pan).to_vec()
    }

    /// The keyboard report holding exactly `usages`.
    fn keys(usages: &[u8]) -> Vec<u8> {
        let mut bitmap = [0; KEYBOARD_BITMAP_LEN];
        for &u in usages {
            set_key(&mut bitmap, u, true);
        }
        keyboard_report(&bitmap).to_vec()
    }

    /// A mouse's counts are the device's; a finished distance and an absolute position are
    /// CGEvent's, which accelerates neither.
    #[test]
    fn counts_go_to_the_device_and_finished_distances_do_not() {
        let mut r = Router::default();
        assert_eq!(r.route(&counts(5, -3)), Some(vec![rel(0, 5, -3, 0, 0)]));
        assert_eq!(r.route(&pixels(5, -3)), None);
        let abs = ev(InputKind::MouseMoveAbs, 0, 10, 10, (100 << 16) | 100);
        assert_eq!(r.route(&abs), None);
        let report = &r.route(&counts(1, 0)).unwrap()[0];
        assert_eq!(report[0], MOUSE_REL_REPORT_ID);
        assert_eq!(report.len(), pf_driver_proto::mouse::MOUSE_REL_REPORT_LEN);
    }

    /// Buttons follow the path that moved the pointer last, and a release follows its press.
    #[test]
    fn buttons_follow_the_pointer_and_releases_follow_their_press() {
        let mut r = Router::default();
        assert_eq!(r.route(&button(1, true)), None, "CGEvent moved it");
        assert_eq!(r.route(&counts(2, 0)), Some(vec![rel(0, 2, 0, 0, 0)]));
        assert_eq!(r.route(&button(1, false)), None, "CGEvent pressed it");
        assert_eq!(r.route(&button(3, true)), Some(vec![rel(0b10, 0, 0, 0, 0)]));
        assert_eq!(
            r.route(&counts(0, 4)),
            Some(vec![rel(0b10, 0, 4, 0, 0)]),
            "a drag carries the button"
        );
        assert_eq!(r.route(&button(3, true)), Some(vec![]), "already down");
        assert_eq!(r.route(&pixels(1, 1)), None);
        assert_eq!(r.route(&button(2, true)), None, "CGEvent moves it now");
        assert_eq!(r.route(&button(3, false)), Some(vec![rel(0, 0, 0, 0, 0)]));
        // Wire 1..=5 are left, middle, right, X1, X2; HID counts primary, secondary, middle.
        r.route(&counts(1, 0));
        for (code, bit) in [(1, 0), (2, 2), (3, 1), (4, 3), (5, 4)] {
            assert_eq!(
                r.route(&button(code, true)),
                Some(vec![rel(1 << bit, 0, 0, 0, 0)])
            );
            r.route(&button(code, false));
        }
    }

    /// Wheel turns go with the pointer, a detent a count; a finger's scroll stays on CGEvent.
    #[test]
    fn wheel_turns_are_detents_and_a_finger_is_not_a_wheel() {
        let mut r = Router::default();
        assert_eq!(r.route(&wheel(ScrollSource::Wheel, 0, 120.0)), None);
        r.route(&counts(1, 0));
        assert_eq!(
            r.route(&wheel(ScrollSource::Wheel, 0, -120.0)),
            Some(vec![rel(0, 0, 0, -1, 0)])
        );
        assert_eq!(r.route(&wheel(ScrollSource::Wheel, 1, 60.0)), Some(vec![]));
        assert_eq!(
            r.route(&wheel(ScrollSource::Wheel, 1, 60.0)),
            Some(vec![rel(0, 0, 0, 0, 1)]),
            "two half detents make one, positive right"
        );
        assert_eq!(r.route(&wheel(ScrollSource::Finger, 0, 6.0)), None);
    }

    /// Keys go to the device whatever moved the pointer; a repeat sends nothing, and a release
    /// it never pressed is CGEvent's.
    #[test]
    fn keys_report_the_held_set() {
        let mut r = Router::default();
        assert_eq!(r.route(&key(0xA0, true)), Some(vec![keys(&[0xE1])]));
        assert_eq!(r.route(&key(0x41, true)), Some(vec![keys(&[0xE1, 0x04])]));
        assert_eq!(r.route(&key(0x41, true)), Some(vec![]), "macOS repeats it");
        assert_eq!(r.route(&key(0x41, false)), Some(vec![keys(&[0xE1])]));
        assert_eq!(r.route(&key(0x42, false)), None, "never pressed here");
        assert_eq!(r.route(&key(0xB3, true)), None, "a media key has no usage");
        assert_eq!(keys(&[])[0], KEYBOARD_REPORT_ID);
    }

    /// Release lets go of buttons and keys in one report each, and only of what is held.
    #[test]
    fn release_lets_go_of_what_is_held() {
        let mut r = Router::default();
        assert!(r.release().is_empty());
        r.route(&counts(1, 0));
        r.route(&button(1, true));
        r.route(&key(0x41, true));
        assert_eq!(r.release(), vec![rel(0, 0, 0, 0, 0), keys(&[])]);
        assert!(r.release().is_empty());
        assert_eq!(r.route(&button(1, false)), None, "released already");
    }
}
