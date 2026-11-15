//! Virtual touchpad ("Punktfunk Touchpad"): a per-session uinput clickpad that libinput turns
//! into pointer motion, scrolling, gestures and clicks under the compositor's own touchpad
//! settings. Gated like the pen: `/dev/uinput` opens, the node is not a seat proxy, and
//! `PUNKTFUNK_TOUCHPAD` is not off. The handshake excludes gamescope.
//!
//! The host's contract, in the order it calls:
//! - [`supported`] and [`taps`] once per session at the handshake: `FEATURE_TOUCHPAD` and
//!   `FEATURE_TOUCHPAD_TAPS`. Taps stay off while the compositor owns tap-to-click.
//! - [`VirtualTouchpad::create`] on the session's first frame. An error latches for the session.
//! - [`VirtualTouchpad::apply`] with the whole pad after every applied frame, in pad units:
//!   X `0..=TOUCHPAD_PAD_MAX_X`, Y `0..=TOUCHPAD_PAD_MAX_Y`, `TOUCHPAD_UNITS_PER_MM` per mm. A
//!   contact absent from the state has lifted; ids are unique and at most 31.
//! - Dropping the value removes the device, at session end and after a final all-lifted apply.
//!
//! `Encoder` writes each state as MT protocol B: one slot per contact for its life, a fresh
//! tracking id per landing, `BTN_TOOL_*` for the count. A contact taken back (`confident`
//! clear) turns `MT_TOOL_PALM` until it lifts: libinput drops its tap and its motion, but ends
//! a scroll or gesture it was part of as a lift would. The name and `1209:5054` stay fixed for
//! compositor rules. Design: `design/touchpad-passthrough.md`.

use crate::uinput_abi::{
    AbsInfo, InputId, UinputDevice, EV_ABS, EV_KEY, EV_SYN, SYN_REPORT, UI_SET_EVBIT,
    UI_SET_KEYBIT, UI_SET_PROPBIT,
};
use anyhow::Result;
use punktfunk_core::quic::{
    PadContact, PadState, ScanClock, TOUCHPAD_MAX_CONTACTS, TOUCHPAD_PAD_MAX_X, TOUCHPAD_PAD_MAX_Y,
    TOUCHPAD_UNITS_PER_MM,
};
use std::time::Instant;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TOOL_TYPE: u16 = 0x37;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const BTN_LEFT: u16 = 0x110;
const BTN_TOOL_FINGER: u16 = 0x145;
const BTN_TOOL_QUINTTAP: u16 = 0x148;
const BTN_TOUCH: u16 = 0x14a;
const BTN_TOOL_DOUBLETAP: u16 = 0x14d;
const BTN_TOOL_TRIPLETAP: u16 = 0x14e;
const BTN_TOOL_QUADTAP: u16 = 0x14f;
const INPUT_PROP_POINTER: u16 = 0x00;
const INPUT_PROP_BUTTONPAD: u16 = 0x02;
const MT_TOOL_FINGER: i32 = 0;
const MT_TOOL_PALM: i32 = 2;

const SLOTS: usize = TOUCHPAD_MAX_CONTACTS;
/// The `BTN_TOOL_*` held for 1..=5 contacts. Without pressure, libinput counts fingers by it.
const TOOLS: [u16; SLOTS] = [
    BTN_TOOL_FINGER,
    BTN_TOOL_DOUBLETAP,
    BTN_TOOL_TRIPLETAP,
    BTN_TOOL_QUADTAP,
    BTN_TOOL_QUINTTAP,
];

const UINPUT: &str = "/dev/uinput";

pub fn supported() -> bool {
    if pf_host_config::env_on("PUNKTFUNK_TOUCHPAD") == Some(false) {
        return false;
    }
    crate::uapi::open_nonblock(UINPUT).is_ok()
        && !crate::pad_broker::proxied(std::path::Path::new(UINPUT))
}

pub fn taps() -> bool {
    false
}

/// `(type, code, value)` as written to the device.
type Event = (u16, u16, i32);

/// One slot's contact, as the kernel holds it.
#[derive(Clone, Copy)]
struct Slot {
    id: u8,
    /// Landing order; the low 16 bits are its tracking id.
    serial: u32,
    x: u16,
    y: u16,
    /// Taken back: `MT_TOOL_PALM` until it lifts.
    palm: bool,
}

/// Pad states to MT protocol B frames. It mirrors the kernel's state and emits only changes.
#[derive(Default)]
struct Encoder {
    slots: [Option<Slot>; SLOTS],
    serial: u32,
    tool: Option<u16>,
    /// Single-touch `ABS_X`/`ABS_Y`: the oldest contact, as the kernel's pointer emulation.
    pointer: (u16, u16),
    button: bool,
}

impl Encoder {
    /// One SYN frame per state. A landing that needs a slot lifting in this state gets a lift
    /// frame first: libinput reads a slot that ends and begins in one frame as one touch.
    fn state(&mut self, pad: &PadState, out: &mut Vec<Event>) {
        let on = pad.contacts();
        let free = self.slots.iter().filter(|s| s.is_none()).count();
        let landing = on.iter().filter(|c| self.slot_of(c.id).is_none()).count();
        if landing > free {
            let kept: Vec<PadContact> = on
                .iter()
                .filter(|c| self.slot_of(c.id).is_some())
                .copied()
                .collect();
            self.frame(self.button, &kept, out);
        }
        self.frame(pad.button, on, out);
    }

    fn slot_of(&self, id: u8) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.is_some_and(|s| s.id == id))
    }

    fn frame(&mut self, button: bool, on: &[PadContact], out: &mut Vec<Event>) {
        let start = out.len();
        // Taken before the lifts below, so no slot ends and begins in this frame.
        let free: Vec<usize> = (0..SLOTS).filter(|&k| self.slots[k].is_none()).collect();
        let mut free = free.into_iter();
        for (k, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_some_and(|s| !on.iter().any(|c| c.id == s.id)) {
                *slot = None;
                out.extend([
                    (EV_ABS, ABS_MT_SLOT, k as i32),
                    (EV_ABS, ABS_MT_TRACKING_ID, -1),
                ]);
            }
        }
        for c in on {
            // Set to the contact's slot below, or dropped when the contact did not change.
            let mark = out.len();
            out.push((EV_ABS, ABS_MT_SLOT, 0));
            let k = if let Some(k) = self.slot_of(c.id) {
                let s = self.slots[k]
                    .as_mut()
                    .expect("slot_of names an occupied slot");
                if s.x != c.x {
                    out.push((EV_ABS, ABS_MT_POSITION_X, c.x.into()));
                }
                if s.y != c.y {
                    out.push((EV_ABS, ABS_MT_POSITION_Y, c.y.into()));
                }
                if !c.confident && !s.palm {
                    out.push((EV_ABS, ABS_MT_TOOL_TYPE, MT_TOOL_PALM));
                }
                (s.x, s.y, s.palm) = (c.x, c.y, s.palm || !c.confident);
                k
            } else if let Some(k) = free.next() {
                self.serial = self.serial.wrapping_add(1);
                let palm = !c.confident;
                let tool = if palm { MT_TOOL_PALM } else { MT_TOOL_FINGER };
                out.extend([
                    (EV_ABS, ABS_MT_TRACKING_ID, (self.serial & 0xffff) as i32),
                    (EV_ABS, ABS_MT_POSITION_X, c.x.into()),
                    (EV_ABS, ABS_MT_POSITION_Y, c.y.into()),
                    (EV_ABS, ABS_MT_TOOL_TYPE, tool),
                ]);
                self.slots[k] = Some(Slot {
                    id: c.id,
                    serial: self.serial,
                    x: c.x,
                    y: c.y,
                    palm,
                });
                k
            } else {
                out.truncate(mark);
                continue;
            };
            if out.len() == mark + 1 {
                out.pop();
            } else {
                out[mark].2 = k as i32;
            }
        }

        let held = self.slots.iter().flatten();
        let tool = held.clone().count().checked_sub(1).map(|i| TOOLS[i]);
        if tool != self.tool {
            out.extend(self.tool.map(|t| (EV_KEY, t, 0)));
            out.extend(tool.map(|t| (EV_KEY, t, 1)));
            if tool.is_some() != self.tool.is_some() {
                out.push((EV_KEY, BTN_TOUCH, tool.is_some().into()));
            }
            self.tool = tool;
        }
        if let Some(s) = held.min_by_key(|s| s.serial) {
            if s.x != self.pointer.0 {
                out.push((EV_ABS, ABS_X, s.x.into()));
            }
            if s.y != self.pointer.1 {
                out.push((EV_ABS, ABS_Y, s.y.into()));
            }
            self.pointer = (s.x, s.y);
        }
        if button != self.button {
            out.push((EV_KEY, BTN_LEFT, button.into()));
            self.button = button;
        }
        if out.len() > start {
            out.push((EV_SYN, SYN_REPORT, 0));
        }
    }
}

/// The least step between two frames' report times: a doubt and its lift share a capture time.
/// libinput drops the tap at 0, 1 or 100 µs alike.
const REPORT_STEP_US: u64 = 1;

/// Report times in µs on `CLOCK_MONOTONIC`: each state at its capture time on the host's clock,
/// so libinput measures the finger's own speed however the frames arrived. Strictly increasing.
#[derive(Default)]
struct ReportClock {
    scan: ScanClock,
    last_us: u64,
}

impl ReportClock {
    /// A state captured at `scan_time` and applied at `now`. `now_us` is read after `now`, so
    /// the result is never in the kernel's future.
    fn stamp(&mut self, scan_time: u16, now: Instant, now_us: u64) -> u64 {
        let at = self.scan.host_time(scan_time, now);
        self.next(now_us.saturating_sub(now.duration_since(at).as_micros() as u64))
    }

    fn next(&mut self, us: u64) -> u64 {
        self.last_us = us.max(self.last_us + REPORT_STEP_US);
        self.last_us
    }
}

/// Per-session uinput touchpad.
pub struct VirtualTouchpad {
    dev: UinputDevice,
    enc: Encoder,
    clock: ReportClock,
    events: Vec<Event>,
}

impl VirtualTouchpad {
    pub fn create() -> Result<VirtualTouchpad> {
        let dev = UinputDevice::open()?;
        dev.set_bits(UI_SET_EVBIT, "UI_SET_EVBIT", &[EV_KEY, EV_ABS])?;
        let mut keys = vec![BTN_LEFT, BTN_TOUCH];
        keys.extend(TOOLS);
        dev.set_bits(UI_SET_KEYBIT, "UI_SET_KEYBIT", &keys)?;
        dev.set_bits(
            UI_SET_PROPBIT,
            "UI_SET_PROPBIT",
            &[INPUT_PROP_POINTER, INPUT_PROP_BUTTONPAD],
        )?;

        // Resolution in units per mm: libinput measures travel, palm and thumb zones in mm.
        let axis = |max: u16| AbsInfo {
            maximum: max.into(),
            resolution: TOUCHPAD_UNITS_PER_MM.into(),
            ..Default::default()
        };
        let range = |maximum: i32| AbsInfo {
            maximum,
            ..Default::default()
        };
        let (x, y) = (axis(TOUCHPAD_PAD_MAX_X), axis(TOUCHPAD_PAD_MAX_Y));
        for (code, info) in [
            (ABS_X, x),
            (ABS_Y, y),
            (ABS_MT_SLOT, range(SLOTS as i32 - 1)),
            (ABS_MT_POSITION_X, x),
            (ABS_MT_POSITION_Y, y),
            (ABS_MT_TOOL_TYPE, range(MT_TOOL_PALM)),
            (ABS_MT_TRACKING_ID, range(0xffff)),
        ] {
            dev.abs(code, info)?;
        }

        // BUS_VIRTUAL: libinput calls the pad internal. It ignores MT_TOOL_PALM on an external one.
        let id = InputId {
            bustype: 0x0006,
            vendor: 0x1209,
            product: 0x5054, // "PT"
            version: 1,
        };
        dev.create(id, b"Punktfunk Touchpad", 0)?;
        tracing::info!("virtual touchpad created (Punktfunk Touchpad, uinput)");
        Ok(VirtualTouchpad {
            dev,
            enc: Encoder::default(),
            clock: ReportClock::default(),
            events: Vec::new(),
        })
    }

    /// Every frame carries the state's capture time. A kernel before 6.5 stamps arrival.
    pub fn apply(&mut self, pad: &PadState) {
        let now = Instant::now();
        let mut time = self.clock.stamp(pad.scan_time, now, crate::monotonic_us());
        self.events.clear();
        self.enc.state(pad, &mut self.events);
        for (i, &(type_, code, value)) in self.events.iter().enumerate() {
            self.dev.emit_at(time, type_, code, value);
            if type_ == EV_SYN && i + 1 < self.events.len() {
                time = self.clock.next(time);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::{touchpad_scan_time, TouchpadContact, TouchpadFrame};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    const SYN: Event = (EV_SYN, SYN_REPORT, 0);

    /// A contact at whole millimetres from the pad's top-left: the surface is the whole pad,
    /// so a millimetre is exactly 100 pad units.
    fn finger(id: u8, x_mm: f32, y_mm: f32) -> TouchpadContact {
        let wire = |mm: f32, span: f32| (mm / span * 65535.0).round() as u16;
        TouchpadContact {
            id,
            tip: true,
            confident: true,
            x: wire(x_mm, 320.0),
            y: wire(y_mm, 240.0),
        }
    }

    fn taken_back(c: TouchpadContact) -> TouchpadContact {
        TouchpadContact {
            confident: false,
            ..c
        }
    }

    fn pad(button: bool, contacts: &[TouchpadContact]) -> PadState {
        stamped(button, contacts, 0)
    }

    fn stamped(button: bool, contacts: &[TouchpadContact], scan_time: u16) -> PadState {
        TouchpadFrame::new(0, button, 3200, 2400, contacts)
            .unwrap()
            .stamped(scan_time)
            .to_pad()
    }

    fn run(e: &mut Encoder, button: bool, contacts: &[TouchpadContact]) -> Vec<Event> {
        let mut out = Vec::new();
        e.state(&pad(button, contacts), &mut out);
        out
    }

    fn ids(e: &Encoder) -> [Option<u8>; SLOTS] {
        e.slots.map(|s| s.map(|s| s.id))
    }

    #[test]
    fn one_finger_lands_moves_and_lifts() {
        let mut e = Encoder::default();
        assert_eq!(
            run(&mut e, false, &[finger(7, 100.0, 50.0)]),
            [
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 10000),
                (EV_ABS, ABS_MT_POSITION_Y, 5000),
                (EV_ABS, ABS_MT_TOOL_TYPE, MT_TOOL_FINGER),
                (EV_KEY, BTN_TOOL_FINGER, 1),
                (EV_KEY, BTN_TOUCH, 1),
                (EV_ABS, ABS_X, 10000),
                (EV_ABS, ABS_Y, 5000),
                SYN,
            ]
        );
        assert_eq!(
            run(&mut e, false, &[finger(7, 101.0, 50.0)]),
            [
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_POSITION_X, 10100),
                (EV_ABS, ABS_X, 10100),
                SYN,
            ]
        );
        assert!(run(&mut e, false, &[finger(7, 101.0, 50.0)]).is_empty());
        assert_eq!(
            run(&mut e, false, &[]),
            [
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_KEY, BTN_TOOL_FINGER, 0),
                (EV_KEY, BTN_TOUCH, 0),
                SYN,
            ]
        );
    }

    #[test]
    fn slots_hold_across_states_and_the_tool_matches_the_count() {
        let mut e = Encoder::default();
        let all: Vec<_> = (0..5u8)
            .map(|i| finger(10 + i, 50.0 + 10.0 * f32::from(i), 50.0))
            .collect();
        for n in 1..=5 {
            run(&mut e, false, &all[..n]);
            assert_eq!(e.tool, Some(TOOLS[n - 1]));
        }
        assert_eq!(ids(&e), [Some(10), Some(11), Some(12), Some(13), Some(14)]);

        let out = run(&mut e, false, &[all[0], all[2], all[4]]);
        assert!(out.ends_with(&[
            (EV_KEY, BTN_TOOL_QUINTTAP, 0),
            (EV_KEY, BTN_TOOL_TRIPLETAP, 1),
            SYN
        ]));
        assert_eq!(ids(&e), [Some(10), None, Some(12), None, Some(14)]);
        // ABS_X follows the oldest contact.
        run(&mut e, false, &[all[2], all[4]]);
        assert_eq!(e.pointer, (7000, 5000));

        let out = run(&mut e, false, &[all[2], all[4], all[0]]);
        assert_eq!(ids(&e), [Some(10), None, Some(12), None, Some(14)]);
        assert!(out.contains(&(EV_ABS, ABS_MT_TRACKING_ID, 6)), "fresh id");
    }

    #[test]
    fn a_landing_never_takes_a_slot_lifting_in_the_same_frame() {
        let mut e = Encoder::default();
        let all: Vec<_> = (0..6u8)
            .map(|i| finger(i, 50.0 + 10.0 * f32::from(i), 50.0))
            .collect();
        run(&mut e, false, &all[..5]);
        let out = run(&mut e, false, &[all[0], all[1], all[2], all[3], all[5]]);
        let syn = out.iter().position(|ev| *ev == SYN).unwrap();
        assert_eq!(
            out[..=syn],
            [
                (EV_ABS, ABS_MT_SLOT, 4),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_KEY, BTN_TOOL_QUINTTAP, 0),
                (EV_KEY, BTN_TOOL_QUADTAP, 1),
                SYN,
            ]
        );
        assert_eq!(
            out[syn + 1..syn + 3],
            [(EV_ABS, ABS_MT_SLOT, 4), (EV_ABS, ABS_MT_TRACKING_ID, 6)]
        );
        assert_eq!(out.iter().filter(|ev| **ev == SYN).count(), 2);
        assert_eq!(ids(&e)[4], Some(5));
    }

    #[test]
    fn the_button_and_a_contact_taken_back() {
        let mut e = Encoder::default();
        let c = finger(0, 100.0, 100.0);
        run(&mut e, false, &[c]);
        assert_eq!(run(&mut e, true, &[c]), [(EV_KEY, BTN_LEFT, 1), SYN]);
        assert_eq!(run(&mut e, false, &[c]), [(EV_KEY, BTN_LEFT, 0), SYN]);
        assert_eq!(
            run(&mut e, false, &[taken_back(c)]),
            [
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TOOL_TYPE, MT_TOOL_PALM),
                SYN
            ]
        );
        assert!(run(&mut e, false, &[c]).is_empty(), "a palm until it lifts");
        run(&mut e, false, &[]);
        let palm = taken_back(finger(1, 10.0, 10.0));
        assert!(run(&mut e, false, &[palm]).contains(&(EV_ABS, ABS_MT_TOOL_TYPE, MT_TOOL_PALM)));
        run(&mut e, false, &[]);
        let next = finger(2, 10.0, 10.0);
        assert!(run(&mut e, false, &[next]).contains(&(EV_ABS, ABS_MT_TOOL_TYPE, MT_TOOL_FINGER)));
    }

    #[test]
    fn report_times_follow_capture_and_only_increase() {
        let (t0, mut c) = (Instant::now(), ReportClock::default());
        let ms = Duration::from_millis;
        assert_eq!(c.stamp(0, t0, 1_000_000), 1_000_000, "at arrival");
        // Captured 8 ms apart, both arriving 16 ms in: spaced as captured.
        assert_eq!(c.stamp(80, t0 + ms(16), 1_016_000), 1_008_000);
        assert_eq!(c.stamp(160, t0 + ms(16), 1_016_000), 1_016_000);
        // A doubt and its lift share a capture time; the lift still comes later.
        let lift = c.stamp(160, t0 + ms(17), 1_017_000);
        assert_eq!(lift, 1_016_000 + REPORT_STEP_US);
        assert_eq!(c.next(5), lift + REPORT_STEP_US, "never backwards");
    }

    /// The kernel keeps the written times (Linux 6.5+): states that arrive together read back
    /// from the event node spaced as they were captured.
    #[test]
    #[ignore = "needs /dev/uinput, the device's /dev/input node and Linux 6.5+"]
    fn a_real_touchpad_reports_capture_times() {
        use crate::uinput_abi::INPUT_EVENT_LEN;
        use std::io::Read;
        use std::os::fd::AsFd;

        let mut pad = VirtualTouchpad::create().expect("create the uinput touchpad");
        let sys = format!("/sys/class/input/{}", pad.dev.sysname().unwrap());
        let node = std::fs::read_dir(sys)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .find(|n| n.starts_with("event"))
            .unwrap();
        sleep(Duration::from_millis(200));
        let mut evdev = crate::uapi::open_nonblock(&format!("/dev/input/{node}")).unwrap();
        // EVIOCSCLOCKID: read back on the clock written.
        let mut clock = libc::CLOCK_MONOTONIC.to_ne_bytes();
        crate::uapi::ioctl_with(evdev.as_fd(), 0x4004_45a0, &mut clock).unwrap();

        let at = |mm: f32| [finger(0, mm, 100.0)];
        pad.apply(&stamped(false, &at(100.0), 0));
        sleep(Duration::from_millis(30));
        for i in 1..=3u16 {
            pad.apply(&stamped(false, &at(100.0 + f32::from(i)), 100 * i));
        }
        let mut buf = [0u8; 64 * INPUT_EVENT_LEN];
        let n = evdev.read(&mut buf).unwrap();
        let word = |ev: &[u8], at: usize| i64::from_ne_bytes(ev[at..at + 8].try_into().unwrap());
        let syn: Vec<i64> = buf[..n]
            .chunks_exact(INPUT_EVENT_LEN)
            .filter(|ev| ev[16..18] == EV_SYN.to_ne_bytes())
            .map(|ev| word(ev, 0) * 1_000_000 + word(ev, 8))
            .collect();
        let gaps: Vec<i64> = syn.windows(2).map(|w| w[1] - w[0]).collect();
        eprintln!("SYN_REPORT at {syn:?} µs, gaps {gaps:?}");
        assert_eq!(gaps.len(), 3);
        assert!(gaps.iter().all(|g| (g - 10_000).abs() < 50), "10 ms apart");
    }

    /// Plays states on a real device, a frame per 8 ms, and names each step on stderr. Attach
    /// in the first five seconds: `libinput debug-events --enable-tap --device /dev/input/eventN`.
    struct Player {
        pad: VirtualTouchpad,
        t0: Instant,
    }

    impl Player {
        fn new() -> Player {
            let pad = VirtualTouchpad::create().expect("create the uinput touchpad");
            let name = pad.dev.sysname().unwrap_or_default();
            eprintln!("created {name}; attach now");
            sleep(Duration::from_secs(5));
            Player {
                pad,
                t0: Instant::now(),
            }
        }

        fn step(&self, name: &str) {
            sleep(Duration::from_millis(1500));
            eprintln!("{:8.3} s  {name}", self.t0.elapsed().as_secs_f32());
        }

        /// The capture time of a state made now, as a client stamps it.
        fn scan(&self) -> u16 {
            touchpad_scan_time(self.t0.elapsed().as_micros() as u64)
        }

        fn show(&mut self, button: bool, contacts: &[TouchpadContact]) {
            self.pad.apply(&stamped(button, contacts, self.scan()));
            sleep(Duration::from_millis(8));
        }

        /// `frames` states moving each contact in a straight line from `from` to `to`.
        fn glide(&mut self, from: &[TouchpadContact], to: &[TouchpadContact], frames: u16) {
            let at = |a: u16, b: u16, i: u16| {
                let t = f32::from(i) / f32::from(frames);
                (f32::from(a) + (f32::from(b) - f32::from(a)) * t) as u16
            };
            for i in 0..=frames {
                let now: Vec<_> = from
                    .iter()
                    .zip(to)
                    .map(|(a, b)| TouchpadContact {
                        x: at(a.x, b.x, i),
                        y: at(a.y, b.y, i),
                        ..*b
                    })
                    .collect();
                self.show(false, &now);
            }
        }

        fn tap(&mut self, contacts: &[TouchpadContact]) {
            self.show(false, contacts);
            sleep(Duration::from_millis(80));
        }
    }

    #[test]
    #[ignore = "needs /dev/uinput; plays gestures for libinput debug-events"]
    fn a_real_touchpad_plays_gestures() {
        let mut p = Player::new();
        p.step("one finger right 60 mm");
        p.glide(&[finger(0, 100.0, 100.0)], &[finger(0, 160.0, 100.0)], 60);
        p.show(false, &[]);

        p.step("two fingers down 40 mm");
        let (a, b) = (finger(0, 140.0, 80.0), finger(1, 170.0, 80.0));
        let down = [finger(0, 140.0, 120.0), finger(1, 170.0, 120.0)];
        p.glide(&[a, b], &down, 40);
        p.show(false, &[]);

        p.step("pinch out, 20 mm to 100 mm apart");
        let pinched = [finger(0, 150.0, 120.0), finger(1, 170.0, 120.0)];
        let spread = [finger(0, 110.0, 120.0), finger(1, 210.0, 120.0)];
        p.glide(&pinched, &spread, 40);
        p.show(false, &[]);

        p.step("three fingers right 50 mm");
        let three = [0, 1, 2].map(|i| finger(i, 120.0 + 15.0 * f32::from(i), 100.0));
        let moved = [0, 1, 2].map(|i| finger(i, 170.0 + 15.0 * f32::from(i), 100.0));
        p.glide(&three, &moved, 50);
        p.show(false, &[]);

        p.step("click with one finger down");
        let c = finger(0, 160.0, 120.0);
        p.show(false, &[c]);
        sleep(Duration::from_millis(100));
        p.show(true, &[c]);
        sleep(Duration::from_millis(150));
        p.show(false, &[c]);
        p.show(false, &[]);

        p.step("a contact taken back from its landing, moving 40 mm");
        let palm = taken_back(finger(0, 120.0, 120.0));
        p.glide(&[palm], &[taken_back(finger(0, 160.0, 120.0))], 40);
        p.show(false, &[]);

        p.step("one finger still 1 s, then right 20 mm");
        p.show(false, &[finger(0, 120.0, 120.0)]);
        sleep(Duration::from_secs(1));
        p.glide(&[finger(0, 120.0, 120.0)], &[finger(0, 140.0, 120.0)], 20);
        p.show(false, &[]);

        p.step("pinch out, taken back midway");
        let half = [finger(0, 130.0, 120.0), finger(1, 190.0, 120.0)];
        p.glide(&pinched, &half, 20);
        p.show(false, &half.map(taken_back));
        p.show(false, &[]);

        p.step("drop");
        drop(p);
        sleep(Duration::from_secs(2));
    }

    #[test]
    #[ignore = "needs /dev/uinput; plays taps for libinput debug-events --enable-tap"]
    fn a_real_touchpad_taps_and_takes_taps_back() {
        let mut p = Player::new();
        let c = finger(0, 160.0, 120.0);
        let pair = [c, finger(1, 180.0, 120.0)];

        p.step("a) tap");
        p.tap(&[c]);
        p.show(false, &[]);

        p.step("b) tap taken back, a frame before the lift");
        p.tap(&[c]);
        p.show(false, &[taken_back(c)]);
        p.show(false, &[]);

        p.step("c) tap taken back, lift back to back, one capture time");
        p.tap(&[c]);
        let scan = p.scan();
        p.pad.apply(&stamped(false, &[taken_back(c)], scan));
        p.pad.apply(&stamped(false, &[], scan));

        p.step("d0) two-finger tap");
        p.tap(&pair);
        p.show(false, &[]);

        p.step("d) two-finger tap taken back");
        p.tap(&pair);
        p.show(false, &pair.map(taken_back));
        p.show(false, &[]);

        p.step("e) tap");
        p.tap(&[c]);
        p.show(false, &[]);

        p.step("e) one finger right 30 mm");
        p.glide(&[c], &[finger(0, 190.0, 120.0)], 30);
        p.show(false, &[]);
        sleep(Duration::from_secs(1));
    }

    /// Sleeps most of the way, then spins: arrivals 0.2 ms apart need better than `sleep`.
    fn wait_until(deadline: Instant) {
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if left > Duration::from_micros(300) {
                sleep(left - Duration::from_micros(200));
            } else {
                std::hint::spin_loop();
            }
        }
    }

    /// Eight 50 mm strokes right at 125 mm/s, captured every 8 ms then every 4 ms, each
    /// delivered evenly, in pairs 1 ms or 0.2 ms apart, or in bursts of four. libinput should
    /// move the pointer 50 mm every time.
    #[test]
    #[ignore = "needs /dev/uinput; plays bunched strokes for libinput debug-events"]
    fn a_real_touchpad_keeps_bunched_strokes_whole() {
        let mut p = Player::new();
        for period in [8000u64, 4000] {
            for (label, group, gap) in [
                ("evenly", 1u64, 0u64),
                ("in pairs 1 ms apart", 2, 1000),
                ("in pairs 0.2 ms apart", 2, 200),
                ("in bursts of four", 4, 0),
            ] {
                p.step(&format!("every {} ms, {label}", period / 1000));
                let frames = 400_000 / period;
                let (start, base) = (Instant::now(), p.t0.elapsed().as_micros() as u64);
                for i in 0..=frames {
                    let last = ((i / group + 1) * group - 1).min(frames);
                    let arrival = last * period + (i % group) * gap;
                    wait_until(start + Duration::from_micros(arrival));
                    let x = 100.0 + (i * period) as f32 * 0.000_125;
                    let scan = touchpad_scan_time(base + i * period);
                    p.pad.apply(&stamped(false, &[finger(0, x, 100.0)], scan));
                }
                p.show(false, &[]);
            }
        }
        sleep(Duration::from_secs(1));
    }
}
